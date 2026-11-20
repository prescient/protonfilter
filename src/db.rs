use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::dump::DumpGame;
use crate::model::{Deck, Dx12Filter, Game, GraphicsApis, Tier, TierSource, WikiLookup};
use crate::pcgw::{NameQuery, Page, page_url};
use crate::protondb::{Summary, game_url};
use crate::steam::StoreItem;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS games (
    appid              INTEGER PRIMARY KEY,
    name               TEXT NOT NULL,
    dump_reports       INTEGER NOT NULL DEFAULT 0,
    dump_comments      INTEGER NOT NULL DEFAULT 0,
    last_report_at     INTEGER,
    tier               TEXT,
    trending_tier      TEXT,
    best_tier          TEXT,
    score              REAL,
    confidence         TEXT,
    reports            INTEGER,
    summary_fetched_at INTEGER,
    on_store           INTEGER,
    native             INTEGER,
    deck               INTEGER,
    steam_fetched_at   INTEGER
);
CREATE TABLE IF NOT EXISTS tags (
    tagid INTEGER PRIMARY KEY,
    name  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS game_tags (
    appid INTEGER NOT NULL,
    tagid INTEGER NOT NULL,
    rank  INTEGER NOT NULL,
    PRIMARY KEY (appid, tagid)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS game_tags_by_tag ON game_tags(tagid);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Columns added after the first release; `init` adds any an older database lacks.
const ADDED_COLUMNS: [(&str, &str); 4] = [
    // Graphics APIs from PCGamingWiki as `GraphicsApis::to_tokens`; NULL if unknown.
    ("apis", "TEXT"),
    ("pcgw_page", "TEXT"),
    ("pcgw_checked_at", "INTEGER"),
    // Whether the app id lookup has run since the last name lookup missed.
    ("pcgw_appid_tried", "INTEGER NOT NULL DEFAULT 0"),
];

/// Report count used for filtering and display: the live ProtonDB total when
/// we have one, otherwise the count from the dump.
const REPORTS_EXPR: &str = "COALESCE(g.reports, g.dump_reports)";

const GAME_COLUMNS: &str = "g.appid, g.name, g.tier, g.trending_tier, g.best_tier, g.score,
    g.confidence, g.on_store, g.native, g.deck, COALESCE(g.reports, g.dump_reports),
    g.dump_comments, g.last_report_at,
    (SELECT group_concat(t.name, char(31) ORDER BY gt.rank)
       FROM game_tags gt JOIN tags t ON t.tagid = gt.tagid
      WHERE gt.appid = g.appid),
    g.apis, g.pcgw_page, g.pcgw_checked_at, g.pcgw_appid_tried";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Reports,
    Comments,
    Score,
    Tier,
    Name,
    /// Most recent report in the dump.
    Recent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum Order {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub genres: Vec<String>,
    pub any_genre: bool,
    /// Only match genres among each game's N most-voted tags.
    pub top_tags: Option<u32>,
    pub exclude_genres: Vec<String>,
    pub tiers: Vec<Tier>,
    pub min_tier: Option<Tier>,
    pub tier_source: TierSource,
    pub exclude_native: bool,
    pub dx12: Dx12Filter,
    /// Leave out games PCGamingWiki has no graphics API info for.
    pub hide_unknown_api: bool,
    pub min_reports: Option<u32>,
    pub max_reports: Option<u32>,
    pub min_comments: Option<u32>,
    pub max_comments: Option<u32>,
    pub deck: Vec<Deck>,
    pub name: Option<String>,
    pub sort: SortKey,
    pub order: Option<Order>,
    /// `None` returns every match.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub games: i64,
    pub with_summary: i64,
    pub with_store_data: i64,
    pub native: i64,
    /// Games whose graphics APIs are known.
    pub with_apis: i64,
    /// Games looked up on PCGamingWiki, found or not.
    pub wiki_checked: i64,
    pub tags: i64,
    pub dump_name: Option<String>,
    pub last_sync: Option<i64>,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        // The GUI and a CLI sync can have the database open at the same time.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;

        // Immediate, so two processes opening an old database don't both add a column.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .prepare("SELECT name FROM pragma_table_info('games')")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, decl) in ADDED_COLUMNS {
            if !existing.iter().any(|c| c == name) {
                tx.execute_batch(&format!("ALTER TABLE games ADD COLUMN {name} {decl}"))
                    .with_context(|| format!("adding column {name}"))?;
            }
        }
        tx.commit()?;
        Ok(Db { conn })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn import_dump<'a>(
        &mut self,
        games: impl IntoIterator<Item = &'a DumpGame>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO games (appid, name, dump_reports, dump_comments, last_report_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(appid) DO UPDATE SET
                    dump_reports   = excluded.dump_reports,
                    dump_comments  = excluded.dump_comments,
                    last_report_at = excluded.last_report_at,
                    name = CASE WHEN games.on_store = 1 THEN games.name ELSE excluded.name END,
                    summary_fetched_at = CASE WHEN games.dump_reports = excluded.dump_reports
                        THEN games.summary_fetched_at END",
            )?;
            for g in games {
                let name = if g.title.is_empty() {
                    format!("App {}", g.appid)
                } else {
                    g.title.clone()
                };
                stmt.execute(params![
                    g.appid,
                    name,
                    g.reports,
                    g.comments,
                    g.last_report_at
                ])?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn replace_tags(&mut self, tags: &[(u32, String)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tags (tagid, name) VALUES (?1, ?2)
                 ON CONFLICT(tagid) DO UPDATE SET name = excluded.name",
            )?;
            for (id, name) in tags {
                stmt.execute(params![id, name])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Apps whose Steam data is missing or older than `stale_before`, most-reported first.
    pub fn apps_needing_store_data(
        &self,
        stale_before: i64,
        min_reports: u32,
        limit: Option<usize>,
    ) -> Result<Vec<u32>> {
        self.stale_apps("steam_fetched_at", stale_before, min_reports, limit)
    }

    /// Apps whose ProtonDB summary is missing or older than `stale_before`, most-reported first.
    pub fn apps_needing_summary(
        &self,
        stale_before: i64,
        min_reports: u32,
        limit: Option<usize>,
    ) -> Result<Vec<u32>> {
        self.stale_apps("summary_fetched_at", stale_before, min_reports, limit)
    }

    fn stale_apps(
        &self,
        column: &str,
        stale_before: i64,
        min_reports: u32,
        limit: Option<usize>,
    ) -> Result<Vec<u32>> {
        let sql = format!(
            "SELECT appid FROM games
              WHERE MAX(dump_reports, COALESCE(reports, 0)) >= ?1
                AND ({column} IS NULL OR {column} < ?2)
              ORDER BY MAX(dump_reports, COALESCE(reports, 0)) DESC, appid
              LIMIT ?3"
        );
        let limit = limit.map_or(-1, |l| l as i64);
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params![min_reports, stale_before, limit], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<u32>>>()?;
        Ok(ids)
    }

    pub fn save_store_items(&mut self, items: &[StoreItem], fetched_at: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut found = tx.prepare(
                "UPDATE games SET name = COALESCE(?2, name), on_store = 1, native = ?3,
                        deck = ?4, steam_fetched_at = ?5
                  WHERE appid = ?1",
            )?;
            let mut missing = tx
                .prepare("UPDATE games SET on_store = 0, steam_fetched_at = ?2 WHERE appid = ?1")?;
            let mut clear_tags = tx.prepare("DELETE FROM game_tags WHERE appid = ?1")?;
            let mut add_tag = tx.prepare(
                "INSERT OR IGNORE INTO game_tags (appid, tagid, rank) VALUES (?1, ?2, ?3)",
            )?;
            for item in items {
                if item.found {
                    found.execute(params![
                        item.appid,
                        item.name,
                        item.native,
                        item.deck,
                        fetched_at
                    ])?;
                    clear_tags.execute([item.appid])?;
                    for (rank, tagid) in item.tagids.iter().enumerate() {
                        add_tag.execute(params![item.appid, tagid, rank as i64])?;
                    }
                } else {
                    missing.execute(params![item.appid, fetched_at])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_summaries(
        &mut self,
        rows: &[(u32, Option<Summary>)],
        fetched_at: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE games SET tier = ?2, trending_tier = ?3, best_tier = ?4, score = ?5,
                        confidence = ?6, reports = ?7, summary_fetched_at = ?8
                  WHERE appid = ?1",
            )?;
            for (appid, summary) in rows {
                let s = summary.as_ref();
                let norm = |t: Option<&String>| t.map(|t| t.to_ascii_lowercase());
                stmt.execute(params![
                    appid,
                    norm(s.and_then(|s| s.tier.as_ref())),
                    norm(s.and_then(|s| s.trending_tier.as_ref())),
                    norm(s.and_then(|s| s.best_reported_tier.as_ref())),
                    s.and_then(|s| s.score),
                    s.and_then(|s| s.confidence.clone()),
                    s.and_then(|s| s.total),
                    fetched_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Games to look up on PCGamingWiki by name (or by the page found last
    /// time): never looked up, or last looked up before `stale_before`.
    pub fn apps_needing_wiki(
        &self,
        stale_before: i64,
        limit: Option<usize>,
    ) -> Result<Vec<NameQuery>> {
        let mut stmt = self.conn.prepare(
            "SELECT appid, name, pcgw_page FROM games
              WHERE pcgw_checked_at IS NULL OR pcgw_checked_at < ?1
              ORDER BY MAX(dump_reports, COALESCE(reports, 0)) DESC, appid
              LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![stale_before, limit.map_or(-1, |l| l as i64)], |r| {
                Ok(NameQuery {
                    appid: r.get(0)?,
                    name: r.get(1)?,
                    known_page: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The name lookup for one game.
    pub fn wiki_query(&self, appid: u32) -> Result<Option<NameQuery>> {
        Ok(self
            .conn
            .query_row(
                "SELECT appid, name, pcgw_page FROM games WHERE appid = ?1",
                [appid],
                |r| {
                    Ok(NameQuery {
                        appid: r.get(0)?,
                        name: r.get(1)?,
                        known_page: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Games whose name found no page, with enough reports to be worth asking
    /// PCGamingWiki by app id (one request each).
    pub fn apps_needing_wiki_appid(
        &self,
        min_reports: u32,
        limit: Option<usize>,
    ) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare(
            "SELECT appid FROM games
              WHERE pcgw_checked_at IS NOT NULL AND pcgw_page IS NULL AND pcgw_appid_tried = 0
                AND MAX(dump_reports, COALESCE(reports, 0)) >= ?1
              ORDER BY MAX(dump_reports, COALESCE(reports, 0)) DESC, appid
              LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(params![min_reports, limit.map_or(-1, |l| l as i64)], |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<Vec<u32>>>()?;
        Ok(ids)
    }

    /// Stores PCGamingWiki lookups. `by_appid` marks the app id lookup as done,
    /// so games it can't find aren't asked about again until they go stale.
    pub fn save_wiki(
        &mut self,
        rows: &[(u32, Option<Page>)],
        by_appid: bool,
        checked_at: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE games SET pcgw_page = ?2, apis = ?3, pcgw_checked_at = ?4, pcgw_appid_tried = ?5
                  WHERE appid = ?1",
            )?;
            for (appid, page) in rows {
                let page = page.as_ref();
                stmt.execute(params![
                    appid,
                    page.map(|p| p.title.as_str()),
                    page.and_then(|p| p.apis.as_ref())
                        .map(GraphicsApis::to_tokens),
                    checked_at,
                    by_appid,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Makes sure a row exists for `appid` so a one-off lookup can be stored.
    pub fn ensure_game(&self, appid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO games (appid, name) VALUES (?1, ?2)",
            params![appid, format!("App {appid}")],
        )?;
        Ok(())
    }

    pub fn game(&self, appid: u32) -> Result<Option<Game>> {
        let sql = format!("SELECT {GAME_COLUMNS} FROM games g WHERE g.appid = ?1");
        Ok(self
            .conn
            .query_row(&sql, [appid], game_from_row)
            .optional()?)
    }

    /// Case-insensitive lookup of tag names; returns the canonical spelling.
    pub fn resolve_tag(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM tags WHERE name = ?1 COLLATE NOCASE",
                [name],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Tags containing `needle`, most-used first, for "did you mean" hints.
    pub fn similar_tags(&self, needle: &str, limit: usize) -> Result<Vec<String>> {
        let pattern = format!("%{}%", escape_like(needle));
        let mut stmt = self.conn.prepare(
            "SELECT t.name FROM tags t LEFT JOIN game_tags gt ON gt.tagid = t.tagid
              WHERE t.name LIKE ?1 ESCAPE '\\'
              GROUP BY t.tagid ORDER BY COUNT(gt.appid) DESC LIMIT ?2",
        )?;
        let names = stmt
            .query_map(params![pattern, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    /// Every tag with the number of games carrying it, most common first.
    pub fn tag_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.name, COUNT(gt.appid) AS n FROM tags t
               JOIN game_tags gt ON gt.tagid = t.tagid
              GROUP BY t.tagid ORDER BY n DESC, t.name",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Largest report and comment counts, for sizing filter sliders.
    pub fn max_counts(&self) -> Result<(u32, u32)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(COALESCE(reports, dump_reports)), 0), COALESCE(MAX(dump_comments), 0)
               FROM games",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn status(&self) -> Result<Status> {
        let (games, with_summary, with_store_data, native, with_apis, wiki_checked) =
            self.conn.query_row(
                "SELECT COUNT(*), COUNT(summary_fetched_at), COUNT(steam_fetched_at),
                        COALESCE(SUM(native = 1), 0), COUNT(apis), COUNT(pcgw_checked_at)
                   FROM games",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )?;
        let tags = self
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
        Ok(Status {
            games,
            with_summary,
            with_store_data,
            native,
            with_apis,
            wiki_checked,
            tags,
            dump_name: self.meta("dump_name")?,
            last_sync: self.meta("last_sync")?.and_then(|s| s.parse().ok()),
        })
    }

    /// Runs a filter. Returns the page of games and the total number of matches.
    pub fn search(&self, f: &Filter) -> Result<(Vec<Game>, usize)> {
        let (where_sql, args) = self.build_where(f)?;

        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM games g WHERE {where_sql}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;

        let order = order_by(f);
        let limit = f.limit.map_or(-1, |l| l as i64);
        let sql = format!(
            "SELECT {GAME_COLUMNS} FROM games g WHERE {where_sql} ORDER BY {order} LIMIT {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let games = stmt
            .query_map(params_from_iter(args.iter()), game_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((games, total as usize))
    }

    fn build_where(&self, f: &Filter) -> Result<(String, Vec<SqlValue>)> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<SqlValue> = Vec::new();
        let tier_col = format!("g.{}", f.tier_source.column());

        // Genres / tags.
        let has_tag = "EXISTS (SELECT 1 FROM game_tags gt JOIN tags t ON t.tagid = gt.tagid
                        WHERE gt.appid = g.appid AND t.name = ? COLLATE NOCASE)";
        if !f.genres.is_empty() {
            let has_top_tag = match f.top_tags {
                Some(n) => has_tag.replace(
                    "COLLATE NOCASE",
                    &format!("COLLATE NOCASE AND gt.rank < {n}"),
                ),
                None => has_tag.to_string(),
            };
            let mut parts = Vec::new();
            for genre in &f.genres {
                args.push(SqlValue::Text(self.require_tag(genre)?));
                parts.push(has_top_tag.clone());
            }
            let joiner = if f.any_genre { " OR " } else { " AND " };
            clauses.push(format!("({})", parts.join(joiner)));
        }
        for genre in &f.exclude_genres {
            args.push(SqlValue::Text(self.require_tag(genre)?));
            clauses.push(format!("NOT {has_tag}"));
        }

        // Tiers: a game matches `native` if it has a Linux build, and a Proton
        // tier if ProtonDB rates it so. Listed tiers are OR'd together.
        if !f.tiers.is_empty() {
            let mut parts = Vec::new();
            if f.tiers.contains(&Tier::Native) {
                parts.push("g.native = 1".to_string());
            }
            let proton: Vec<Tier> = f
                .tiers
                .iter()
                .copied()
                .filter(|t| *t != Tier::Native)
                .collect();
            if !proton.is_empty() {
                parts.push(format!("{tier_col} IN ({})", placeholders(proton.len())));
                args.extend(proton.iter().map(|t| SqlValue::Text(t.as_str().into())));
            }
            clauses.push(format!("({})", parts.join(" OR ")));
        }
        if let Some(min) = f.min_tier {
            let ok: Vec<Tier> = Tier::PROTON.into_iter().filter(|t| *t >= min).collect();
            if ok.is_empty() {
                clauses.push("g.native = 1".into());
            } else {
                clauses.push(format!(
                    "(g.native = 1 OR {tier_col} IN ({}))",
                    placeholders(ok.len())
                ));
                args.extend(ok.iter().map(|t| SqlValue::Text(t.as_str().into())));
            }
        }
        if f.exclude_native {
            clauses.push("g.native IS NOT 1".into());
        }

        // Graphics APIs. `dx12` alone means DirectX 12 is the only option.
        match f.dx12 {
            Dx12Filter::Any => {}
            Dx12Filter::NotRequired => clauses.push("g.apis IS NOT 'dx12'".into()),
            Dx12Filter::Exclude => {
                clauses.push("(g.apis IS NULL OR ',' || g.apis || ',' NOT LIKE '%,dx12,%')".into())
            }
        }
        if f.hide_unknown_api {
            clauses.push("g.apis IS NOT NULL".into());
        }

        // Counts.
        let mut range = |expr: &str, op: &str, v: Option<u32>| {
            if let Some(v) = v {
                clauses.push(format!("{expr} {op} ?"));
                args.push(SqlValue::Integer(v.into()));
            }
        };
        range(REPORTS_EXPR, ">=", f.min_reports);
        range(REPORTS_EXPR, "<=", f.max_reports);
        range("g.dump_comments", ">=", f.min_comments);
        range("g.dump_comments", "<=", f.max_comments);

        if !f.deck.is_empty() {
            let mut parts = vec![format!("g.deck IN ({})", placeholders(f.deck.len()))];
            args.extend(f.deck.iter().map(|d| SqlValue::Integer(d.code())));
            if f.deck.contains(&Deck::Unknown) {
                parts.push("g.deck IS NULL".into());
            }
            clauses.push(format!("({})", parts.join(" OR ")));
        }

        if let Some(name) = f.name.as_deref().filter(|n| !n.is_empty()) {
            clauses.push("g.name LIKE ? ESCAPE '\\'".into());
            args.push(SqlValue::Text(format!("%{}%", escape_like(name))));
        }

        if clauses.is_empty() {
            clauses.push("1".into());
        }
        Ok((clauses.join(" AND "), args))
    }

    fn require_tag(&self, name: &str) -> Result<String> {
        if let Some(canonical) = self.resolve_tag(name)? {
            return Ok(canonical);
        }
        let has_tags: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
        if has_tags == 0 {
            bail!("no genre data yet; run `protonfilter sync` first");
        }
        let hints = self.similar_tags(name, 8)?;
        if hints.is_empty() {
            bail!("unknown genre or tag '{name}'. Run `protonfilter genres --all` to list them.");
        }
        bail!(
            "unknown genre or tag '{name}'. Did you mean: {}?",
            hints.join(", ")
        )
    }
}

fn order_by(f: &Filter) -> String {
    let tier_col = format!("g.{}", f.tier_source.column());
    let (expr, default_order) = match f.sort {
        SortKey::Reports => (REPORTS_EXPR.to_string(), Order::Desc),
        SortKey::Comments => ("g.dump_comments".to_string(), Order::Desc),
        SortKey::Score => ("g.score".to_string(), Order::Desc),
        SortKey::Name => ("g.name COLLATE NOCASE".to_string(), Order::Asc),
        SortKey::Recent => ("g.last_report_at".to_string(), Order::Desc),
        SortKey::Tier => (
            format!(
                "CASE WHEN g.native = 1 THEN 6 ELSE CASE {tier_col}
                    WHEN 'platinum' THEN 5 WHEN 'gold' THEN 4 WHEN 'silver' THEN 3
                    WHEN 'bronze' THEN 2 WHEN 'borked' THEN 1 WHEN 'pending' THEN 0 END END"
            ),
            Order::Desc,
        ),
    };
    let dir = match f.order.unwrap_or(default_order) {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    };
    // Unknown values always sink to the bottom; ties go to the more-reported game.
    format!("({expr}) IS NULL, {expr} {dir}, {REPORTS_EXPR} DESC, g.appid")
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn game_from_row(r: &Row) -> rusqlite::Result<Game> {
    let appid: u32 = r.get(0)?;
    let on_store: Option<bool> = r.get(7)?;
    let native: Option<bool> = r.get(8)?;
    let deck: Option<i64> = r.get(9)?;
    let tags: Option<String> = r.get(13)?;
    let apis: Option<String> = r.get(14)?;
    let pcgw_page: Option<String> = r.get(15)?;
    let wiki_checked_at: Option<i64> = r.get(16)?;
    let wiki_appid_tried: bool = r.get(17)?;
    let wiki_lookup = match (wiki_checked_at, &pcgw_page) {
        (None, _) => WikiLookup::Pending,
        (Some(_), Some(_)) => WikiLookup::Found,
        (Some(_), None) if wiki_appid_tried => WikiLookup::NotFound,
        (Some(_), None) => WikiLookup::NameMissed,
    };
    Ok(Game {
        appid,
        name: r.get(1)?,
        tier: r.get(2)?,
        trending_tier: r.get(3)?,
        best_tier: r.get(4)?,
        score: r.get(5)?,
        confidence: r.get(6)?,
        // Only trust the Linux flag if Steam actually gave us store data.
        native: if on_store == Some(true) { native } else { None },
        deck: if on_store == Some(true) {
            deck.map(Deck::from_code)
        } else {
            None
        },
        reports: r.get(10)?,
        comments: r.get(11)?,
        last_report_at: r.get(12)?,
        tags: tags
            .map(|t| t.split('\u{1f}').map(str::to_string).collect())
            .unwrap_or_default(),
        graphics_apis: apis
            .map(|a| GraphicsApis::from_tokens(&a))
            .filter(|a| !a.is_empty()),
        protondb_url: game_url(appid),
        pcgamingwiki_url: pcgw_page.as_deref().map(page_url),
        wiki_lookup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(tier: &str, total: u32) -> Option<Summary> {
        Some(Summary {
            tier: Some(tier.into()),
            trending_tier: Some(tier.into()),
            best_reported_tier: Some("platinum".into()),
            score: Some(0.5),
            confidence: Some("strong".into()),
            total: Some(total),
        })
    }

    fn item(appid: u32, native: bool, deck: i64, tagids: &[u32]) -> StoreItem {
        StoreItem {
            appid,
            found: true,
            name: None,
            native,
            deck,
            tagids: tagids.to_vec(),
        }
    }

    /// 1: native RPG, proton gold, 300 reports, 120 comments
    /// 2: RPG+Strategy, platinum, 50 reports, 40 comments
    /// 3: Action, borked, 5 reports, 1 comment
    /// 4: Strategy, pending, no Steam data
    fn fixture() -> Db {
        let mut db = Db::open_in_memory().unwrap();
        db.replace_tags(&[
            (19, "Action".into()),
            (122, "RPG".into()),
            (9, "Strategy".into()),
            (1716, "Roguelike".into()),
            (3959, "Roguelite".into()),
        ])
        .unwrap();
        let dump = [
            DumpGame {
                appid: 1,
                title: "Alpha Quest".into(),
                reports: 280,
                comments: 120,
                last_report_at: Some(300),
            },
            DumpGame {
                appid: 2,
                title: "Beta Tactics".into(),
                reports: 45,
                comments: 40,
                last_report_at: Some(200),
            },
            DumpGame {
                appid: 3,
                title: "Gamma_Shooter".into(),
                reports: 5,
                comments: 1,
                last_report_at: Some(100),
            },
            DumpGame {
                appid: 4,
                title: "Delta Empire".into(),
                reports: 2,
                comments: 0,
                last_report_at: Some(400),
            },
        ];
        db.import_dump(&dump).unwrap();
        db.save_store_items(
            &[
                item(1, true, 3, &[122, 19]),
                item(2, false, 2, &[9, 122]),
                item(3, false, 1, &[19, 1716]),
                StoreItem {
                    appid: 4,
                    found: false,
                    name: None,
                    native: false,
                    deck: 0,
                    tagids: vec![9],
                },
            ],
            1000,
        )
        .unwrap();
        db.save_summaries(
            &[
                (1, summary("gold", 300)),
                (2, summary("platinum", 50)),
                (3, summary("borked", 5)),
                (4, summary("pending", 2)),
            ],
            1000,
        )
        .unwrap();
        db
    }

    fn ids(db: &Db, f: Filter) -> Vec<u32> {
        db.search(&f).unwrap().0.iter().map(|g| g.appid).collect()
    }

    #[test]
    fn default_sort_is_reports_desc() {
        assert_eq!(ids(&fixture(), Filter::default()), vec![1, 2, 3, 4]);
    }

    #[test]
    fn genre_filters() {
        let db = fixture();
        let f = |g: &[&str], any| Filter {
            genres: g.iter().map(|s| s.to_string()).collect(),
            any_genre: any,
            ..Default::default()
        };
        assert_eq!(ids(&db, f(&["rpg"], false)), vec![1, 2], "case-insensitive");
        assert_eq!(
            ids(&db, f(&["RPG", "Strategy"], false)),
            vec![2],
            "all must match"
        );
        assert_eq!(
            ids(&db, f(&["RPG", "Strategy"], true)),
            vec![1, 2],
            "--any-genre"
        );
        let excl = Filter {
            exclude_genres: vec!["Action".into()],
            ..Default::default()
        };
        assert_eq!(ids(&db, excl), vec![2, 4]);
        let top = Filter {
            genres: vec!["RPG".into()],
            top_tags: Some(1),
            ..Default::default()
        };
        assert_eq!(
            ids(&db, top),
            vec![1],
            "app 2 has RPG only as its second tag"
        );
    }

    #[test]
    fn unknown_genre_suggests_alternatives() {
        let err = fixture()
            .search(&Filter {
                genres: vec!["rogue".into()],
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("Roguelike"), "{err}");
    }

    #[test]
    fn tier_filters() {
        let db = fixture();
        let tiers = |t: &[Tier]| Filter {
            tiers: t.to_vec(),
            ..Default::default()
        };
        assert_eq!(ids(&db, tiers(&[Tier::Native])), vec![1]);
        assert_eq!(ids(&db, tiers(&[Tier::Platinum])), vec![2]);
        assert_eq!(
            ids(&db, tiers(&[Tier::Gold])),
            vec![1],
            "native games keep their Proton tier"
        );
        assert_eq!(ids(&db, tiers(&[Tier::Native, Tier::Borked])), vec![1, 3]);

        let min = |t| Filter {
            min_tier: Some(t),
            ..Default::default()
        };
        assert_eq!(
            ids(&db, min(Tier::Platinum)),
            vec![1, 2],
            "native ranks above platinum"
        );
        assert_eq!(ids(&db, min(Tier::Bronze)), vec![1, 2]);
        assert_eq!(ids(&db, min(Tier::Native)), vec![1]);

        let proton_only = Filter {
            min_tier: Some(Tier::Gold),
            exclude_native: true,
            ..Default::default()
        };
        assert_eq!(ids(&db, proton_only), vec![2]);

        let best = Filter {
            tiers: vec![Tier::Platinum],
            tier_source: TierSource::Best,
            ..Default::default()
        };
        assert_eq!(ids(&db, best), vec![1, 2, 3, 4]);
    }

    #[test]
    fn count_filters_use_live_reports_and_dump_comments() {
        let db = fixture();
        let f = Filter {
            min_reports: Some(50),
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![1, 2], "app 2 has 45 in dump but 50 live");
        let f = Filter {
            min_comments: Some(10),
            max_comments: Some(100),
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![2]);
        let f = Filter {
            max_reports: Some(5),
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![3, 4]);
    }

    #[test]
    fn deck_and_name_filters() {
        let db = fixture();
        let f = Filter {
            deck: vec![Deck::Verified, Deck::Playable],
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![1, 2]);
        let f = Filter {
            name: Some("a_s".into()),
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![3]);
        let f = Filter {
            name: Some("a_q".into()),
            ..Default::default()
        };
        assert_eq!(
            ids(&db, f),
            Vec::<u32>::new(),
            "underscore is literal, not a wildcard"
        );
        let f = Filter {
            name: Some("TACTICS".into()),
            ..Default::default()
        };
        assert_eq!(ids(&db, f), vec![2]);
    }

    #[test]
    fn sorting_and_limits() {
        let db = fixture();
        let sorted = |sort, order| Filter {
            sort,
            order,
            ..Default::default()
        };
        assert_eq!(ids(&db, sorted(SortKey::Name, None)), vec![1, 2, 4, 3]);
        assert_eq!(
            ids(&db, sorted(SortKey::Comments, Some(Order::Asc))),
            vec![4, 3, 2, 1]
        );
        assert_eq!(ids(&db, sorted(SortKey::Tier, None)), vec![1, 2, 3, 4]);
        assert_eq!(ids(&db, sorted(SortKey::Recent, None)), vec![4, 1, 2, 3]);

        let (games, total) = db
            .search(&Filter {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((games.len(), total), (2, 4));
    }

    #[test]
    fn game_rows_carry_tags_in_rank_order() {
        let db = fixture();
        let g = db.game(1).unwrap().unwrap();
        assert_eq!(g.tags, vec!["RPG", "Action"]);
        assert_eq!(g.native, Some(true));
        assert_eq!(g.deck, Some(Deck::Verified));
        assert_eq!(g.tier.as_deref(), Some("gold"));

        let missing = db.game(4).unwrap().unwrap();
        assert_eq!(
            missing.native, None,
            "no Steam data means unknown, not 'not native'"
        );
        assert!(missing.tags.is_empty());
    }

    #[test]
    fn stale_queries_and_reimport() {
        let mut db = fixture();
        assert_eq!(
            db.apps_needing_summary(2000, 0, None).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            db.apps_needing_summary(500, 0, None).unwrap(),
            Vec::<u32>::new()
        );
        assert_eq!(db.apps_needing_summary(2000, 10, Some(1)).unwrap(), vec![1]);

        // A re-import keeps the Steam name for listed games.
        db.save_store_items(
            &[StoreItem {
                name: Some("Alpha Quest: GOTY".into()),
                ..item(1, true, 3, &[122])
            }],
            1000,
        )
        .unwrap();
        db.import_dump(&[DumpGame {
            appid: 1,
            title: "alpha quest".into(),
            reports: 999,
            comments: 5,
            last_report_at: None,
        }])
        .unwrap();
        let g = db.game(1).unwrap().unwrap();
        assert_eq!(g.name, "Alpha Quest: GOTY");
        assert_eq!(g.comments, 5);
        assert_eq!(
            db.apps_needing_summary(500, 0, None).unwrap(),
            vec![1],
            "new reports in the dump mark the rating stale"
        );
    }

    fn page(title: &str, tokens: &str) -> Option<Page> {
        Some(Page {
            title: title.into(),
            steam_appids: vec![],
            apis: Some(GraphicsApis::from_tokens(tokens)).filter(|a| !a.is_empty()),
        })
    }

    #[test]
    fn dx12_filters() {
        let mut db = fixture();
        // 1: DX11 + DX12, 2: DX12 only, 3: DX11, 4: unknown.
        db.save_wiki(
            &[
                (1, page("Alpha Quest", "dx11,dx12")),
                (2, page("Beta Tactics", "dx12")),
                (3, page("Gamma Shooter", "dx11,vulkan")),
                (4, None),
            ],
            false,
            1000,
        )
        .unwrap();
        let f = |dx12, hide_unknown_api| Filter {
            dx12,
            hide_unknown_api,
            ..Default::default()
        };
        assert_eq!(ids(&db, f(Dx12Filter::Any, false)), vec![1, 2, 3, 4]);
        assert_eq!(
            ids(&db, f(Dx12Filter::NotRequired, false)),
            vec![1, 3, 4],
            "only the DX12-only game goes; unknown stays"
        );
        assert_eq!(ids(&db, f(Dx12Filter::Exclude, false)), vec![3, 4]);
        assert_eq!(ids(&db, f(Dx12Filter::Exclude, true)), vec![3]);
        assert_eq!(ids(&db, f(Dx12Filter::Any, true)), vec![1, 2, 3]);

        let g = db.game(3).unwrap().unwrap();
        assert_eq!(g.graphics_apis.unwrap().short_label(), "DX11 + VK");
        assert_eq!(
            g.pcgamingwiki_url.as_deref(),
            Some("https://www.pcgamingwiki.com/wiki/Gamma_Shooter")
        );
        assert_eq!(db.status().unwrap().with_apis, 3);
    }

    #[test]
    fn wiki_lookup_stages() {
        let mut db = fixture();
        let names = |db: &Db, stale| {
            db.apps_needing_wiki(stale, None)
                .unwrap()
                .into_iter()
                .map(|q| (q.appid, q.known_page))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            db.game(1).unwrap().unwrap().wiki_lookup,
            WikiLookup::Pending
        );
        assert_eq!(names(&db, 0).len(), 4, "nothing looked up yet");

        // By name: 1 found, the rest missed.
        db.save_wiki(
            &[
                (1, page("Alpha Quest", "dx11")),
                (2, None),
                (3, None),
                (4, None),
            ],
            false,
            1000,
        )
        .unwrap();
        assert_eq!(names(&db, 1000), vec![], "all fresh");
        assert_eq!(
            db.game(2).unwrap().unwrap().wiki_lookup,
            WikiLookup::NameMissed
        );
        assert_eq!(
            db.apps_needing_wiki_appid(5, None).unwrap(),
            vec![2, 3],
            "app id lookups only for games with enough reports"
        );

        // By app id: 2 found, 3 not.
        db.save_wiki(
            &[(2, page("Beta Tactics (2020)", "dx12")), (3, None)],
            true,
            1000,
        )
        .unwrap();
        assert_eq!(
            db.apps_needing_wiki_appid(5, None).unwrap(),
            Vec::<u32>::new()
        );
        assert_eq!(db.game(2).unwrap().unwrap().wiki_lookup, WikiLookup::Found);
        assert_eq!(
            db.game(3).unwrap().unwrap().wiki_lookup,
            WikiLookup::NotFound
        );

        // Once stale, games are looked up again, under the page found last time.
        let stale = names(&db, 2000);
        assert_eq!(stale.len(), 4);
        assert!(stale.contains(&(2, Some("Beta Tactics (2020)".into()))));
        assert_eq!(db.status().unwrap().wiki_checked, 4);
    }

    #[test]
    fn opening_an_old_database_adds_the_new_columns() {
        let path = std::env::temp_dir().join(format!("protonfilter-old-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute("INSERT INTO games (appid, name) VALUES (7, 'Old Game')", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let g = db.game(7).unwrap().unwrap();
        assert_eq!(
            (g.graphics_apis, g.wiki_lookup),
            (None, WikiLookup::Pending)
        );
        drop(db);
        Db::open(&path).expect("opening again is a no-op");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
