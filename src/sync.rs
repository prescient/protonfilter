use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::Client;

use crate::db::Db;
use crate::http::Pacer;
use crate::model::now_unix;
use crate::{dump, pcgw, protondb, steam};

/// Graphics API info rarely changes, so PCGamingWiki lookups are redone less
/// often than ratings.
pub const WIKI_STALE_DAYS: i64 = 90;
/// Requests per second to PCGamingWiki, which is run by volunteers.
const WIKI_RATE: f64 = 1.0;
/// App id lookups per save. Each is a request, so this also bounds how long
/// a stop request takes to land.
const APPID_CHUNK: usize = 20;
/// Give up on PCGamingWiki for this sync after this many failures in a row.
const WIKI_MAX_FAILURES: usize = 3;

pub struct SyncOptions {
    /// Import this local dump instead of downloading the latest one.
    pub dump: Option<PathBuf>,
    pub skip_dump: bool,
    pub force_dump: bool,
    pub skip_steam: bool,
    pub skip_protondb: bool,
    /// Skip PCGamingWiki graphics API lookups.
    pub skip_wiki: bool,
    /// Only look games up on PCGamingWiki by app id (one request each) if
    /// they have at least this many reports. Lookups by name cover every game.
    pub wiki_min_reports: u32,
    pub concurrency: usize,
    /// ProtonDB requests per second.
    pub rate: f64,
    pub stale_days: u32,
    pub min_reports: u32,
    pub limit: Option<usize>,
    pub cache_dir: PathBuf,
    /// When set, progress and messages go here instead of the terminal (used by the GUI).
    pub progress: Option<std::sync::Arc<SyncProgress>>,
}

impl SyncOptions {
    pub fn new(cache_dir: PathBuf) -> Self {
        SyncOptions {
            dump: None,
            skip_dump: false,
            force_dump: false,
            skip_steam: false,
            skip_protondb: false,
            skip_wiki: false,
            wiki_min_reports: 10,
            concurrency: 4,
            rate: 12.0,
            stale_days: 30,
            min_reports: 0,
            limit: None,
            cache_dir,
            progress: None,
        }
    }

    fn say(&self, msg: String) {
        match &self.progress {
            Some(p) => p.log.lock().expect("progress lock").push(msg),
            None => println!("{msg}"),
        }
    }

    fn warn(&self, pb: &ProgressBar, msg: String) {
        if self.progress.is_some() {
            self.say(msg);
        } else if pb.is_hidden() {
            // Bars are hidden when stderr isn't a terminal, and so is anything printed through them.
            eprintln!("{msg}");
        } else {
            pb.println(msg);
        }
    }

    fn cancelled(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.is_cancelled())
    }

    fn bar(&self, len: u64, msg: String, unit: Unit) -> ProgressBar {
        let template = match unit {
            Unit::Bytes => {
                "{msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})"
            }
            Unit::Games => "{msg} [{bar:30.cyan/blue}] {pos}/{len} ({per_sec}, {eta})",
        };
        let pb = match &self.progress {
            Some(_) => ProgressBar::with_draw_target(Some(len), ProgressDrawTarget::hidden()),
            None => ProgressBar::new(len),
        };
        pb.set_style(
            ProgressStyle::with_template(template)
                .expect("valid template")
                .progress_chars("=> "),
        );
        pb.set_message(msg);
        if let Some(p) = &self.progress {
            *p.bar.lock().expect("progress lock") = Some((pb.clone(), unit));
        }
        pb
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Games,
}

/// Live view of a running sync for a GUI: the current step's progress, the
/// messages printed so far, and a cancel switch.
#[derive(Default)]
pub struct SyncProgress {
    bar: Mutex<Option<(ProgressBar, Unit)>>,
    log: Mutex<Vec<String>>,
    cancel: AtomicBool,
}

pub struct ProgressSnapshot {
    pub message: String,
    pub position: u64,
    pub length: u64,
    pub unit: Unit,
}

impl SyncProgress {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn current(&self) -> Option<ProgressSnapshot> {
        let bar = self.bar.lock().expect("progress lock");
        let (pb, unit) = bar.as_ref()?;
        if pb.is_finished() {
            return None;
        }
        Some(ProgressSnapshot {
            message: pb.message(),
            position: pb.position(),
            length: pb.length().unwrap_or(0),
            unit: *unit,
        })
    }

    pub fn messages(&self) -> Vec<String> {
        self.log.lock().expect("progress lock").clone()
    }
}

pub async fn run(db: &mut Db, client: &Client, opts: &SyncOptions) -> Result<()> {
    import_dump(db, client, opts).await?;

    let stale_before = now_unix() - i64::from(opts.stale_days) * 86_400;
    if !opts.skip_steam && !opts.cancelled() {
        sync_store(db, client, stale_before, opts).await?;
    }
    if !opts.skip_protondb && !opts.cancelled() {
        sync_summaries(db, client, stale_before, opts).await?;
    }
    if !opts.skip_wiki && !opts.cancelled() {
        sync_wiki(db, opts).await?;
    }
    if opts.cancelled() {
        opts.say("Sync stopped. Run it again to pick up where it left off.".into());
    } else {
        db.set_meta("last_sync", &now_unix().to_string())?;
    }
    Ok(())
}

/// Fetches fresh ProtonDB and Steam data for one game and stores it. Fails if
/// neither knows the app and it isn't already in the database.
pub async fn refresh_game(db: &mut Db, client: &Client, appid: u32) -> Result<()> {
    let existed = db.game(appid)?.is_some();
    let ids = [appid];
    let pacer = Pacer::per_second(10.0);
    let need_tags = db.status()?.tags == 0;
    let (summary, items, tags) = tokio::join!(
        protondb::fetch_summary(client, &pacer, appid),
        steam::fetch_items(client, &ids),
        async {
            if need_tags {
                Some(steam::fetch_tags(client).await)
            } else {
                None
            }
        },
    );
    let summary = summary.context("fetching ProtonDB rating")?;
    let items = items.context("fetching Steam store data")?;
    let on_store = items.iter().any(|i| i.found);
    if !existed && summary.is_none() && !on_store {
        bail!("no ProtonDB or Steam data for app {appid}");
    }
    if let Some(Ok(tags)) = tags {
        db.replace_tags(&tags)?;
    }
    db.ensure_game(appid)?;
    let now = now_unix();
    if summary.is_some() {
        db.save_summaries(&[(appid, summary)], now)?;
    }
    db.save_store_items(&items, now)?;
    lookup_wiki_game(db, appid)
        .await
        .context("looking the game up on PCGamingWiki")
}

/// Looks one game up on PCGamingWiki: under its known page or Steam name,
/// then by app id.
pub async fn lookup_wiki_game(db: &mut Db, appid: u32) -> Result<()> {
    let Some(query) = db.wiki_query(appid)? else {
        bail!("app {appid} isn't in the database");
    };
    let client = pcgw::client()?;
    let pacer = Pacer::per_second(5.0);
    let rows = pcgw::find_by_name(&client, &pacer, std::slice::from_ref(&query)).await?;
    if rows.iter().any(|(_, page)| page.is_some()) {
        return db.save_wiki(&rows, false, now_unix());
    }
    let rows = pcgw::find_by_appid(&client, &pacer, &[appid]).await?;
    db.save_wiki(&rows, true, now_unix())
}

async fn import_dump(db: &mut Db, client: &Client, opts: &SyncOptions) -> Result<()> {
    let (path, name, downloaded) = if let Some(path) = &opts.dump {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        (path.clone(), name, false)
    } else if opts.skip_dump {
        return Ok(());
    } else {
        let latest = dump::latest(client).await?;
        if !opts.force_dump && db.meta("dump_name")?.as_deref() == Some(latest.name.as_str()) {
            opts.say(format!("ProtonDB dump {} is already imported", latest.name));
            return Ok(());
        }
        std::fs::create_dir_all(&opts.cache_dir)
            .with_context(|| format!("creating {}", opts.cache_dir.display()))?;
        let dest = opts.cache_dir.join(&latest.name);
        let pb = opts.bar(0, format!("Downloading {}", latest.name), Unit::Bytes);
        dump::download(client, &latest.url, &dest, &pb).await?;
        pb.finish_and_clear();
        (dest, latest.name, true)
    };

    let pb = opts.bar(0, format!("Reading {name}"), Unit::Bytes);
    let games = {
        let (path, pb) = (path.clone(), pb.clone());
        tokio::task::spawn_blocking(move || dump::parse_file(&path, &pb)).await??
    };
    pb.finish_and_clear();

    let reports: u64 = games.values().map(|g| u64::from(g.reports)).sum();
    let comments: u64 = games.values().map(|g| u64::from(g.comments)).sum();
    let n = db.import_dump(games.values())?;
    db.set_meta("dump_name", &name)?;
    if downloaded {
        // Only the per-game totals are kept; the 70 MB archive isn't needed again.
        let _ = std::fs::remove_file(&path);
    }
    opts.say(format!(
        "Imported {n} games from {name} ({reports} reports, {comments} with comments)"
    ));
    Ok(())
}

async fn sync_store(
    db: &mut Db,
    client: &Client,
    stale_before: i64,
    opts: &SyncOptions,
) -> Result<()> {
    let tags = steam::fetch_tags(client).await?;
    db.replace_tags(&tags)?;

    let ids = db.apps_needing_store_data(stale_before, opts.min_reports, opts.limit)?;
    if ids.is_empty() {
        opts.say("Steam store data is up to date".into());
        return Ok(());
    }

    let pb = opts.bar(ids.len() as u64, "Steam store data".into(), Unit::Games);
    let mut results = futures::stream::iter(ids.chunks(steam::BATCH_SIZE))
        .map(|chunk| async move { (chunk.len(), steam::fetch_items(client, chunk).await) })
        .buffer_unordered(3);

    let (mut done, mut failed) = (0, 0);
    while let Some((len, result)) = results.next().await {
        match result {
            Ok(items) => db.save_store_items(&items, now_unix())?,
            Err(e) => {
                failed += len;
                opts.warn(&pb, format!("warning: {e:#}"));
            }
        }
        done += len;
        pb.inc(len as u64);
        if opts.cancelled() {
            break;
        }
    }
    pb.finish_and_clear();
    report(opts, "Steam store data", done, failed);
    Ok(())
}

async fn sync_summaries(
    db: &mut Db,
    client: &Client,
    stale_before: i64,
    opts: &SyncOptions,
) -> Result<()> {
    let ids = db.apps_needing_summary(stale_before, opts.min_reports, opts.limit)?;
    if ids.is_empty() {
        opts.say("ProtonDB ratings are up to date".into());
        return Ok(());
    }

    let pb = opts.bar(ids.len() as u64, "ProtonDB ratings".into(), Unit::Games);
    let pacer = &Pacer::per_second(opts.rate);
    let mut results = futures::stream::iter(ids.iter().copied())
        .map(|appid| async move { (appid, protondb::fetch_summary(client, pacer, appid).await) })
        .buffer_unordered(opts.concurrency.max(1))
        .ready_chunks(200);

    let (mut done, mut failed) = (0, 0);
    while let Some(batch) = results.next().await {
        let len = batch.len();
        let mut ok = Vec::with_capacity(len);
        for (appid, result) in batch {
            match result {
                Ok(summary) => ok.push((appid, summary)),
                Err(e) => {
                    failed += 1;
                    if failed <= 5 {
                        opts.warn(&pb, format!("warning: {e:#}"));
                    }
                }
            }
        }
        db.save_summaries(&ok, now_unix())?;
        done += len;
        pb.inc(len as u64);
        if opts.cancelled() {
            break;
        }
    }
    pb.finish_and_clear();
    report(opts, "ProtonDB ratings", done, failed);
    Ok(())
}

/// Finds each game's PCGamingWiki page and reads its graphics APIs: by name
/// for every game (50 per request), then by app id for well-reported games
/// the names missed.
async fn sync_wiki(db: &mut Db, opts: &SyncOptions) -> Result<()> {
    let client = pcgw::client()?;
    let pacer = Pacer::per_second(WIKI_RATE);
    let stale_before = now_unix() - WIKI_STALE_DAYS * 86_400;

    let queries = db.apps_needing_wiki(stale_before, opts.limit)?;
    if queries.is_empty() {
        opts.say("PCGamingWiki pages are up to date".into());
    } else {
        let pb = opts.bar(
            queries.len() as u64,
            "PCGamingWiki pages".into(),
            Unit::Games,
        );
        let mut tally = WikiTally::default();
        for batch in pcgw::name_batches(queries) {
            let result = pcgw::find_by_name(&client, &pacer, &batch).await;
            if !tally.record(db, opts, &pb, batch.len(), result, false)? {
                break;
            }
        }
        pb.finish_and_clear();
        tally.report(opts, "PCGamingWiki", "by name");
        if tally.gave_up() {
            return Ok(());
        }
    }
    if opts.cancelled() {
        return Ok(());
    }

    let ids = db.apps_needing_wiki_appid(opts.wiki_min_reports, opts.limit)?;
    if !ids.is_empty() {
        let pb = opts.bar(ids.len() as u64, "PCGamingWiki app ids".into(), Unit::Games);
        let mut tally = WikiTally::default();
        for chunk in ids.chunks(APPID_CHUNK) {
            let result = pcgw::find_by_appid(&client, &pacer, chunk).await;
            if !tally.record(db, opts, &pb, chunk.len(), result, true)? {
                break;
            }
        }
        pb.finish_and_clear();
        tally.report(opts, "PCGamingWiki", "by Steam app id");
    }
    let s = db.status()?;
    opts.say(format!(
        "Graphics APIs are known for {} of {} games",
        s.with_apis, s.games
    ));
    Ok(())
}

#[derive(Default)]
struct WikiTally {
    done: usize,
    found: usize,
    failed: usize,
    failures_in_a_row: usize,
}

impl WikiTally {
    /// Saves one batch's results. Returns false when the step should stop.
    fn record(
        &mut self,
        db: &mut Db,
        opts: &SyncOptions,
        pb: &ProgressBar,
        len: usize,
        result: Result<Vec<(u32, Option<pcgw::Page>)>>,
        by_appid: bool,
    ) -> Result<bool> {
        match result {
            Ok(rows) => {
                self.found += rows.iter().filter(|(_, page)| page.is_some()).count();
                db.save_wiki(&rows, by_appid, now_unix())?;
                self.failures_in_a_row = 0;
            }
            Err(e) => {
                self.failed += len;
                self.failures_in_a_row += 1;
                opts.warn(pb, format!("warning: {e:#}"));
            }
        }
        self.done += len;
        pb.inc(len as u64);
        Ok(!opts.cancelled() && !self.gave_up())
    }

    fn gave_up(&self) -> bool {
        self.failures_in_a_row >= WIKI_MAX_FAILURES
    }

    fn report(&self, opts: &SyncOptions, what: &str, how: &str) {
        let checked = self.done - self.failed;
        opts.say(format!(
            "{what}: found {} pages for {checked} games looked up {how}",
            self.found
        ));
        if self.gave_up() {
            opts.say(format!(
                "{what} kept failing, so the rest was skipped; it will be retried on the next sync"
            ));
        } else if self.failed > 0 {
            opts.say(format!(
                "{what}: {} games failed (they'll be retried on the next sync)",
                self.failed
            ));
        }
    }
}

fn report(opts: &SyncOptions, what: &str, done: usize, failed: usize) {
    if failed == 0 {
        opts.say(format!("{what}: updated {done} games"));
    } else {
        opts.say(format!(
            "{what}: updated {} games, {failed} failed (they'll be retried on the next sync)",
            done - failed
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_progress_sees_the_current_step_and_messages() {
        let progress = std::sync::Arc::new(SyncProgress::default());
        let mut opts = SyncOptions::new(PathBuf::from("/nonexistent"));
        opts.progress = Some(progress.clone());
        assert!(progress.current().is_none());

        let pb = opts.bar(10, "ProtonDB ratings".into(), Unit::Games);
        pb.inc(3);
        let now = progress.current().expect("a step is running");
        assert_eq!(
            (now.message.as_str(), now.position, now.length),
            ("ProtonDB ratings", 3, 10)
        );
        assert_eq!(now.unit, Unit::Games);

        opts.say("hello".into());
        report(&opts, "Steam store data", 5, 2);
        assert_eq!(
            progress.messages(),
            vec![
                "hello".to_string(),
                "Steam store data: updated 3 games, 2 failed (they'll be retried on the next sync)"
                    .to_string(),
            ]
        );

        pb.finish_and_clear();
        assert!(progress.current().is_none(), "finished steps aren't shown");

        assert!(!opts.cancelled());
        progress.cancel();
        assert!(opts.cancelled());
    }
}
