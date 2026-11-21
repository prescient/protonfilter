mod db;
mod dump;
mod gui;
mod http;
mod model;
mod output;
mod pcgw;
mod protondb;
mod steam;
mod sync;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use directories::ProjectDirs;

use crate::db::{Db, Filter, Order, SortKey};
use crate::model::{Deck, Dx12Filter, STEAM_GENRES, Tier, TierSource};
use crate::output::Format;

/// Filter ProtonDB games by genre, compatibility tier, and report/comment counts.
///
/// Run `protonfilter sync` once to build the local database, then query it
/// with `protonfilter search`, or browse it with `protonfilter gui`.
#[derive(Parser)]
#[command(name = "protonfilter", version)]
struct Cli {
    /// Database file [default: ~/.local/share/protonfilter/protonfilter.db]
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open the graphical filter window
    Gui,
    /// Download ProtonDB and Steam data into the local database
    Sync(SyncArgs),
    /// Filter games by genre, tier, reports and comments
    #[command(visible_alias = "filter")]
    Search(SearchArgs),
    /// Show everything known about one game (fetches fresh data for it)
    Show {
        /// Steam app id, or part of the game's name
        game: String,
        /// Don't fetch fresh data; use the database as-is
        #[arg(long)]
        offline: bool,
    },
    /// List genres and tags you can filter on, with game counts
    Genres {
        /// List every Steam tag, not just the main genres
        #[arg(long, short)]
        all: bool,
        /// Only tags whose name contains this text
        #[arg(long, short)]
        search: Option<String>,
    },
    /// Show what's in the database
    Status,
}

#[derive(Args)]
struct SyncArgs {
    /// Import a local dump (.tar.gz or .json) instead of downloading the latest
    #[arg(long, value_name = "PATH")]
    dump: Option<PathBuf>,
    /// Skip the dump step and only refresh ratings and store data
    #[arg(long, conflicts_with = "dump")]
    skip_dump: bool,
    /// Re-import the latest dump even if it was already imported
    #[arg(long)]
    force_dump: bool,
    /// Skip Steam store data (genres, native, Deck)
    #[arg(long)]
    skip_steam: bool,
    /// Skip ProtonDB ratings
    #[arg(long)]
    skip_protondb: bool,
    /// Skip PCGamingWiki graphics API lookups (DirectX, Vulkan, OpenGL)
    #[arg(long)]
    skip_pcgw: bool,
    /// Every game is looked up on PCGamingWiki by name. Games it misses are
    /// also looked up by app id, one request each, if they have this many reports
    #[arg(long, default_value_t = 10, value_name = "N")]
    pcgw_min_reports: u32,
    /// Parallel requests to ProtonDB
    #[arg(long, short, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32))]
    concurrency: u16,
    /// ProtonDB requests per second. ProtonDB blocks clients for a minute above ~16/s
    #[arg(long, default_value_t = 12.0)]
    rate: f64,
    /// Refresh ratings and store data older than this many days. Games with new
    /// reports in a newer dump are refreshed regardless
    #[arg(long, default_value_t = 30)]
    stale_days: u32,
    /// Only fetch ratings and store data for games with at least this many reports
    #[arg(long, default_value_t = 0)]
    min_reports: u32,
    /// Only process this many games per step, most-reported first (handy for a quick test)
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Args)]
struct SearchArgs {
    /// Genre or Steam tag; repeat or comma-separate. Games must match all of them
    #[arg(long, short, value_delimiter = ',', value_name = "GENRE")]
    genre: Vec<String>,
    /// Match games with any of the --genre values instead of all
    #[arg(long)]
    any_genre: bool,
    /// Only match --genre against each game's N most-voted Steam tags (1-20).
    /// Lower is stricter: e.g. 5 stops a shooter matching "Strategy"
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=20))]
    top_tags: Option<u32>,
    /// Leave out games with this genre or tag; repeat or comma-separate
    #[arg(long, short = 'x', value_delimiter = ',', value_name = "GENRE")]
    exclude_genre: Vec<String>,
    /// Only these tiers; repeat or comma-separate. `native` means a Linux build exists
    #[arg(long, short, value_delimiter = ',', value_name = "TIER")]
    tier: Vec<Tier>,
    /// This tier or better (native > platinum > gold > silver > bronze > borked > pending)
    #[arg(long, short, value_name = "TIER")]
    min_tier: Option<Tier>,
    /// Which ProtonDB tier to filter and sort on
    #[arg(long, value_enum, default_value_t = TierSource::Current)]
    tier_source: TierSource,
    /// Leave out games with a native Linux build
    #[arg(long)]
    exclude_native: bool,
    /// DirectX 12 games run through VKD3D-Proton. `not-required` hides games that
    /// need DX12; `exclude` hides every game that supports it. From PCGamingWiki
    #[arg(long, value_enum, default_value_t = Dx12Filter::Any)]
    dx12: Dx12Filter,
    /// Leave out games whose graphics APIs aren't known
    #[arg(long)]
    hide_unknown_api: bool,
    /// Only games with a Vulkan renderer. From PCGamingWiki
    #[arg(long)]
    vulkan: bool,
    /// At least this many reports
    #[arg(long, short = 'r', value_name = "N")]
    min_reports: Option<u32>,
    /// At most this many reports
    #[arg(long, value_name = "N")]
    max_reports: Option<u32>,
    /// At least this many reports with written comments
    #[arg(long, short = 'c', value_name = "N")]
    min_comments: Option<u32>,
    /// At most this many reports with written comments
    #[arg(long, value_name = "N")]
    max_comments: Option<u32>,
    /// Steam Deck status; repeat or comma-separate
    #[arg(long, short, value_delimiter = ',', value_name = "STATUS")]
    deck: Vec<Deck>,
    /// Name contains this text
    #[arg(long, short)]
    name: Option<String>,
    /// Sort by
    #[arg(long, short, value_enum, default_value_t = SortKey::Reports)]
    sort: SortKey,
    /// Sort direction [default: desc, or asc for name]
    #[arg(long, short, value_enum)]
    order: Option<Order>,
    /// Maximum rows to show; 0 shows all
    #[arg(long, short, default_value_t = 50)]
    limit: usize,
    /// Output format
    #[arg(long, short, value_enum, default_value_t = Format::Table)]
    format: Format,
}

impl From<SearchArgs> for (Filter, Format) {
    fn from(a: SearchArgs) -> Self {
        let filter = Filter {
            genres: a.genre,
            any_genre: a.any_genre,
            top_tags: a.top_tags,
            exclude_genres: a.exclude_genre,
            tiers: a.tier,
            min_tier: a.min_tier,
            tier_source: a.tier_source,
            exclude_native: a.exclude_native,
            dx12: a.dx12,
            hide_unknown_api: a.hide_unknown_api,
            vulkan: a.vulkan,
            min_reports: a.min_reports,
            max_reports: a.max_reports,
            min_comments: a.min_comments,
            max_comments: a.max_comments,
            deck: a.deck,
            name: a.name,
            sort: a.sort,
            order: a.order,
            limit: (a.limit > 0).then_some(a.limit),
        };
        (filter, a.format)
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        // `protonfilter search | head` closing the pipe early isn't an error.
        let broken_pipe = e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
        });
        if !broken_pipe {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let dirs = ProjectDirs::from("", "", "protonfilter")
        .context("couldn't determine a home directory; pass --db")?;
    let db_path = cli
        .db
        .unwrap_or_else(|| dirs.data_dir().join("protonfilter.db"));
    let cache_dir = dirs.cache_dir().to_path_buf();

    // The window needs the main thread, so it runs outside the async runtime.
    if let Command::Gui = cli.command {
        return gui::run(db_path, cache_dir);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting async runtime")?
        .block_on(run_command(cli.command, db_path, cache_dir))
}

async fn run_command(command: Command, db_path: PathBuf, cache_dir: PathBuf) -> Result<()> {
    let mut db = Db::open(&db_path)?;

    match command {
        Command::Gui => unreachable!("handled in run()"),
        Command::Sync(a) => {
            let client = http::client()?;
            let opts = sync::SyncOptions {
                dump: a.dump,
                skip_dump: a.skip_dump,
                force_dump: a.force_dump,
                skip_steam: a.skip_steam,
                skip_protondb: a.skip_protondb,
                skip_wiki: a.skip_pcgw,
                wiki_min_reports: a.pcgw_min_reports,
                concurrency: a.concurrency.into(),
                rate: a.rate,
                stale_days: a.stale_days,
                min_reports: a.min_reports,
                limit: a.limit,
                ..sync::SyncOptions::new(cache_dir)
            };
            sync::run(&mut db, &client, &opts).await?;
        }
        Command::Search(a) => {
            let (filter, format) = a.into();
            let (games, total) = db.search(&filter)?;
            if total == 0 && db.status()?.games == 0 {
                bail!("the database is empty; run `protonfilter sync` first");
            }
            output::games(&games, total, format)?;
        }
        Command::Show { game, offline } => show(&mut db, &game, offline).await?,
        Command::Genres { all, search } => {
            let mut counts = db.tag_counts()?;
            if counts.is_empty() {
                bail!("no genre data yet; run `protonfilter sync` first");
            }
            let total_tags = counts.len();
            if let Some(needle) = search.as_deref().map(str::to_lowercase) {
                counts.retain(|(name, _)| name.to_lowercase().contains(&needle));
            } else if !all {
                counts.retain(|(name, _)| STEAM_GENRES.contains(&name.as_str()));
            }
            output::tag_counts(&counts);
            if !all && search.is_none() {
                eprintln!(
                    "These are Steam's main genres. Any of the {total_tags} Steam tags \
                     (e.g. Roguelike, Metroidvania) also works with --genre; see `genres --all`."
                );
            }
        }
        Command::Status => output::status(&db.status()?, &db_path),
    }
    Ok(())
}

async fn show(db: &mut Db, query: &str, offline: bool) -> Result<()> {
    let appid = match query.trim().parse::<u32>() {
        Ok(id) => id,
        Err(_) => match find_by_name(db, query)? {
            Some(id) => id,
            None => return Ok(()),
        },
    };

    if !offline {
        let client = http::client()?;
        let refreshed = sync::refresh_game(db, &client, appid).await;
        if let Err(e) = refreshed {
            if db.game(appid)?.is_none() {
                return Err(e);
            }
            eprintln!("warning: couldn't fetch fresh data: {e:#}");
        }
    }

    match db.game(appid)? {
        Some(game) => {
            output::game_details(&game, db.meta("dump_name")?.as_deref());
            Ok(())
        }
        None => bail!("app {appid} isn't in the database; try without --offline"),
    }
}

/// Resolves a name fragment to one app id, or prints the candidates.
fn find_by_name(db: &Db, query: &str) -> Result<Option<u32>> {
    let filter = Filter {
        name: Some(query.to_string()),
        limit: Some(15),
        ..Default::default()
    };
    let (games, total) = db.search(&filter)?;
    if let Some(exact) = games
        .iter()
        .find(|g| g.name.eq_ignore_ascii_case(query.trim()))
    {
        return Ok(Some(exact.appid));
    }
    match games.as_slice() {
        [] => bail!("no game in the database matches '{query}'"),
        [only] => Ok(Some(only.appid)),
        _ => {
            eprintln!("'{query}' matches {total} games; pass an app id:");
            output::games(&games, total, Format::Table)?;
            Ok(None)
        }
    }
}
