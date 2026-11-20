use std::io::{self, Write};

use anyhow::Result;
use clap::ValueEnum;
use comfy_table::{Cell, CellAlignment, Color, ContentArrangement, Table, presets};

use crate::db::Status;
use crate::model::{Deck, Game, GraphicsApis, WikiLookup, format_date};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Format {
    #[default]
    Table,
    Json,
    Csv,
}

pub fn games(games: &[Game], total: usize, format: Format) -> Result<()> {
    let mut out = io::stdout().lock();
    match format {
        Format::Table => {
            if games.is_empty() {
                writeln!(out, "No games match those filters.")?;
                return Ok(());
            }
            writeln!(out, "{}", table(games))?;
        }
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, games).map_err(io::Error::from)?;
            writeln!(out)?;
        }
        Format::Csv => write_csv(&mut out, games)?,
    }
    if games.len() < total {
        eprintln!(
            "Showing {} of {total} matching games (use --limit 0 to show all)",
            games.len()
        );
    } else if format == Format::Table {
        eprintln!("{total} matching games");
    }
    Ok(())
}

fn table(games: &[Game]) -> Table {
    let mut t = Table::new();
    t.load_style(presets::UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "AppID", "Name", "Tier", "Native", "Deck", "API", "Reports", "Comments", "Tags",
        ]);
    for g in games {
        t.add_row([
            Cell::new(g.appid),
            Cell::new(truncate(&g.name, 42)),
            tier_cell(g.tier.as_deref()),
            native_cell(g.native),
            deck_cell(g.deck),
            api_cell(g.graphics_apis.as_ref()),
            Cell::new(g.reports).set_alignment(CellAlignment::Right),
            Cell::new(g.comments).set_alignment(CellAlignment::Right),
            Cell::new(
                g.tags
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ]);
    }
    t
}

fn write_csv(out: &mut impl Write, games: &[Game]) -> Result<()> {
    let mut w = csv::Writer::from_writer(out);
    w.write_record([
        "appid",
        "name",
        "tier",
        "trending_tier",
        "best_tier",
        "score",
        "confidence",
        "native",
        "deck",
        "reports",
        "comments",
        "last_report",
        "tags",
        "graphics_apis",
        "protondb_url",
        "pcgamingwiki_url",
    ])
    .map_err(csv_error)?;
    for g in games {
        let opt = |s: &Option<String>| s.clone().unwrap_or_default();
        w.write_record([
            g.appid.to_string(),
            g.name.clone(),
            opt(&g.tier),
            opt(&g.trending_tier),
            opt(&g.best_tier),
            g.score.map(|s| s.to_string()).unwrap_or_default(),
            opt(&g.confidence),
            g.native.map(|n| n.to_string()).unwrap_or_default(),
            g.deck.map(|d| d.as_str().to_string()).unwrap_or_default(),
            g.reports.to_string(),
            g.comments.to_string(),
            g.last_report_at.map(format_date).unwrap_or_default(),
            g.tags.join(";"),
            g.graphics_apis
                .as_ref()
                .map(|a| a.to_tokens().replace(',', ";"))
                .unwrap_or_default(),
            g.protondb_url.clone(),
            g.pcgamingwiki_url.clone().unwrap_or_default(),
        ])
        .map_err(csv_error)?;
    }
    w.flush()?;
    Ok(())
}

/// Unwraps I/O errors so callers can recognise a closed pipe.
fn csv_error(e: csv::Error) -> anyhow::Error {
    match e.into_kind() {
        csv::ErrorKind::Io(io) => io.into(),
        other => anyhow::anyhow!("writing CSV: {other:?}"),
    }
}

pub fn game_details(g: &Game, dump_name: Option<&str>) {
    let dash = |s: &Option<String>| s.clone().unwrap_or_else(|| "-".into());
    println!("{} ({})", g.name, g.appid);
    println!("  Tier         {}", dash(&g.tier));
    println!("  Trending     {}", dash(&g.trending_tier));
    println!("  Best report  {}", dash(&g.best_tier));
    if let Some(score) = g.score {
        println!(
            "  Score        {score:.2} ({} confidence)",
            dash(&g.confidence)
        );
    }
    println!("  Native       {}", native_text(g.native));
    println!("  Steam Deck   {}", g.deck.map_or("-", Deck::as_str));
    println!("  Graphics     {}", api_text(g));
    println!("  Reports      {}", g.reports);
    match dump_name {
        Some(d) => println!("  Comments     {} (as of {d})", g.comments),
        None => println!("  Comments     {}", g.comments),
    }
    if let Some(ts) = g.last_report_at {
        println!("  Last report  {}", format_date(ts));
    }
    if !g.tags.is_empty() {
        println!("  Tags         {}", g.tags.join(", "));
    }
    println!("  ProtonDB     {}", g.protondb_url);
    if let Some(url) = &g.pcgamingwiki_url {
        println!("  PCGamingWiki {url}");
    }
    println!(
        "  Steam        https://store.steampowered.com/app/{}",
        g.appid
    );
}

pub fn tag_counts(rows: &[(String, i64)]) {
    let mut t = Table::new();
    t.load_style(presets::UTF8_BORDERS_ONLY)
        .set_header(["Genre / tag", "Games"]);
    for (name, n) in rows {
        t.add_row([
            Cell::new(name),
            Cell::new(n).set_alignment(CellAlignment::Right),
        ]);
    }
    println!("{t}");
}

pub fn status(s: &Status, db_path: &std::path::Path) {
    println!("Database      {}", db_path.display());
    println!("Games         {}", s.games);
    println!("  rated       {}", s.with_summary);
    println!("  store data  {}", s.with_store_data);
    println!("  native      {}", s.native);
    println!(
        "  graphics    {} (looked up on PCGamingWiki: {})",
        s.with_apis, s.wiki_checked
    );
    println!("Tags          {}", s.tags);
    println!(
        "Dump          {}",
        s.dump_name.as_deref().unwrap_or("never imported")
    );
    match s.last_sync {
        Some(ts) => println!("Last sync     {}", format_date(ts)),
        None => println!("Last sync     never (run `protonfilter sync`)"),
    }
}

fn tier_cell(tier: Option<&str>) -> Cell {
    let Some(tier) = tier else {
        return Cell::new("-");
    };
    let color = match tier {
        "platinum" => Color::Cyan,
        "gold" => Color::Yellow,
        "silver" => Color::Grey,
        "bronze" => Color::DarkYellow,
        "borked" => Color::Red,
        _ => Color::DarkGrey,
    };
    Cell::new(tier).fg(color)
}

fn native_cell(native: Option<bool>) -> Cell {
    match native {
        Some(true) => Cell::new("yes").fg(Color::Green),
        other => Cell::new(native_text(other)),
    }
}

fn native_text(native: Option<bool>) -> &'static str {
    match native {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    }
}

fn api_cell(apis: Option<&GraphicsApis>) -> Cell {
    match apis {
        Some(a) if a.needs_dx12() => Cell::new(a.short_label()).fg(Color::Red),
        Some(a) => Cell::new(a.short_label()),
        None => Cell::new("?"),
    }
}

fn api_text(g: &Game) -> String {
    match (&g.graphics_apis, g.wiki_lookup) {
        (Some(a), _) if a.needs_dx12() => {
            format!("{} (DX12 only: runs through VKD3D-Proton)", a.label())
        }
        (Some(a), _) => a.label(),
        (None, WikiLookup::Found) => "unknown (PCGamingWiki doesn't list it)".into(),
        (None, WikiLookup::Pending) => {
            "unknown (not looked up yet; run `protonfilter sync`)".into()
        }
        (None, _) => "unknown (no PCGamingWiki page)".into(),
    }
}

fn deck_cell(deck: Option<Deck>) -> Cell {
    match deck {
        Some(Deck::Verified) => Cell::new("verified").fg(Color::Green),
        Some(Deck::Playable) => Cell::new("playable").fg(Color::Yellow),
        Some(Deck::Unsupported) => Cell::new("unsupported").fg(Color::Red),
        _ => Cell::new("-"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
