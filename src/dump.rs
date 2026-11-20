//! The monthly ProtonDB report export published at
//! https://github.com/bdefore/protondb-data. It's the only public source that
//! lists every game with reports, and it includes each report's written notes,
//! which is where the per-game comment count comes from.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use indicatif::ProgressBar;
use reqwest::Client;
use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::http::send_with_retry;

const LISTING_URL: &str = "https://api.github.com/repos/bdefore/protondb-data/contents/reports";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpInfo {
    pub name: String,
    pub url: String,
}

/// Per-game totals pulled out of the dump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DumpGame {
    pub appid: u32,
    pub title: String,
    pub reports: u32,
    pub comments: u32,
    pub last_report_at: Option<i64>,
}

#[derive(Deserialize)]
struct ListingEntry {
    name: String,
    download_url: Option<String>,
}

/// Finds the newest `reports_<mon><day>_<year>.tar.gz` in the repo.
pub async fn latest(client: &Client) -> Result<DumpInfo> {
    let resp = send_with_retry(|| {
        client
            .get(LISTING_URL)
            .header("Accept", "application/vnd.github+json")
    })
    .await?;
    if !resp.status().is_success() {
        bail!(
            "GitHub returned HTTP {} listing ProtonDB dumps",
            resp.status()
        );
    }
    let entries: Vec<ListingEntry> = resp.json().await.context("parsing GitHub listing")?;
    entries
        .into_iter()
        .filter_map(|e| {
            let date = dump_date(&e.name)?;
            Some((
                date,
                DumpInfo {
                    url: e.download_url?,
                    name: e.name,
                },
            ))
        })
        .max_by_key(|(date, _)| *date)
        .map(|(_, info)| info)
        .ok_or_else(|| anyhow!("no report dumps found in {LISTING_URL}"))
}

/// `reports_nov1_2026.tar.gz` -> `(2026, 11, 1)`.
pub fn dump_date(name: &str) -> Option<(u32, u32, u32)> {
    let stem = name.strip_prefix("reports_")?.strip_suffix(".tar.gz")?;
    let (month_day, year) = stem.split_once('_')?;
    let split = month_day.find(|c: char| c.is_ascii_digit())?;
    let (month, day) = month_day.split_at(split);
    let month = match month.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some((year.parse().ok()?, month, day.parse().ok()?))
}

pub async fn download(client: &Client, url: &str, dest: &Path, pb: &ProgressBar) -> Result<()> {
    let resp = send_with_retry(|| client.get(url)).await?;
    if !resp.status().is_success() {
        bail!("HTTP {} downloading {url}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        pb.set_length(len);
    }
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download")?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// Parses a dump from disk: either the `.tar.gz` as published or the bare
/// `.json` inside it. `pb` tracks uncompressed bytes read.
pub fn parse_file(path: &Path, pb: &ProgressBar) -> Result<HashMap<u32, DumpGame>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let is_json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json {
        pb.set_length(file.metadata()?.len());
        return parse_reports(BufReader::with_capacity(1 << 20, pb.wrap_read(file)));
    }

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
    for entry in archive.entries().context("reading dump archive")? {
        let entry = entry?;
        let is_json = entry
            .path()?
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if is_json {
            pb.set_length(entry.header().size()?);
            return parse_reports(BufReader::with_capacity(1 << 20, pb.wrap_read(entry)));
        }
    }
    bail!("no .json file inside {}", path.display())
}

/// Streams a JSON array of reports and tallies them per app.
pub fn parse_reports<R: Read>(reader: R) -> Result<HashMap<u32, DumpGame>> {
    let mut games = HashMap::new();
    let mut de = serde_json::Deserializer::from_reader(reader);
    serde::Deserializer::deserialize_seq(&mut de, ReportsVisitor(&mut games))
        .context("parsing ProtonDB report dump")?;
    de.end().context("trailing data after report array")?;
    Ok(games)
}

// Only the fields we need; everything else (system info, answers) is skipped.
// Types are loose on purpose so one odd record can't abort a 500 MB parse.
#[derive(Deserialize)]
struct RawReport {
    #[serde(default)]
    app: Option<RawApp>,
    #[serde(default)]
    responses: Option<RawResponses>,
    #[serde(default)]
    timestamp: Option<Value>,
}

#[derive(Deserialize)]
struct RawApp {
    #[serde(default)]
    steam: Option<RawSteam>,
    #[serde(default)]
    title: Option<Value>,
}

#[derive(Deserialize)]
struct RawSteam {
    #[serde(rename = "appId", default)]
    app_id: Option<Value>,
}

#[derive(Deserialize)]
struct RawResponses {
    #[serde(default)]
    notes: Option<Value>,
}

struct ReportsVisitor<'a>(&'a mut HashMap<u32, DumpGame>);

impl<'de> Visitor<'de> for ReportsVisitor<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an array of ProtonDB reports")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(report) = seq.next_element::<RawReport>()? {
            tally(self.0, report);
        }
        Ok(())
    }
}

fn tally(games: &mut HashMap<u32, DumpGame>, report: RawReport) {
    let Some(app) = report.app else { return };
    let Some(appid) = app.steam.and_then(|s| s.app_id).as_ref().and_then(as_u32) else {
        return;
    };
    let ts = report.timestamp.as_ref().and_then(as_i64);
    let has_comment = report
        .responses
        .and_then(|r| r.notes)
        .is_some_and(|n| has_text(&n));
    let title = app.title.as_ref().and_then(Value::as_str).map(str::trim);

    let game = games.entry(appid).or_insert_with(|| DumpGame {
        appid,
        ..Default::default()
    });
    game.reports += 1;
    if has_comment {
        game.comments += 1;
    }
    // Keep the title from the newest report; games get renamed.
    let newest = match (ts, game.last_report_at) {
        (Some(t), Some(last)) => t >= last,
        (Some(_), None) => true,
        (None, _) => game.title.is_empty(),
    };
    if newest && ts.is_some() {
        game.last_report_at = ts;
    }
    if let Some(title) = title.filter(|t| !t.is_empty())
        && (newest || game.title.is_empty())
    {
        game.title = title.to_string();
    }
}

fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// True if the notes contain any non-blank text, whatever their shape.
fn has_text(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(map) => map.values().any(has_text),
        Value::Array(items) => items.iter().any(has_text),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"app": {"steam": {"appId": "352620"}, "title": "Porcunipine"},
       "responses": {"notes": {"extra": "oh noes", "verdict": "is le borked"}, "verdict": "no"},
       "timestamp": 1572299227, "systemInfo": {"cpu": "x"}},
      {"app": {"steam": {"appId": "352620"}, "title": "Porcunipine Remastered"},
       "responses": {"verdict": "yes"},
       "timestamp": 1600000000},
      {"app": {"steam": {"appId": 219990}, "title": "Grim Dawn"},
       "responses": {"notes": {"verdict": "   "}},
       "timestamp": "1572304395"},
      {"app": {"steam": {"appId": "219990"}, "title": "Grim Dawn (old name)"},
       "responses": {"notes": {"extra": "", "verdict": "runs great"}},
       "timestamp": 1500000000},
      {"app": {"title": "Not a Steam game"}, "responses": {"notes": {"verdict": "hi"}}},
      {"app": {"steam": {"appId": "abc"}}, "responses": {}},
      {"unexpected": true}
    ]"#;

    #[test]
    fn tallies_reports_and_comments() {
        let games = parse_reports(SAMPLE.as_bytes()).unwrap();
        assert_eq!(games.len(), 2);

        let p = &games[&352620];
        assert_eq!((p.reports, p.comments), (2, 1));
        assert_eq!(p.title, "Porcunipine Remastered");
        assert_eq!(p.last_report_at, Some(1_600_000_000));

        let g = &games[&219990];
        assert_eq!((g.reports, g.comments), (2, 1), "blank notes don't count");
        assert_eq!(g.title, "Grim Dawn", "title comes from the newest report");
        assert_eq!(g.last_report_at, Some(1_572_304_395));
    }

    #[test]
    fn rejects_non_array() {
        assert!(parse_reports(&b"{}"[..]).is_err());
    }

    #[test]
    fn parses_dump_names() {
        assert_eq!(dump_date("reports_nov1_2026.tar.gz"), Some((2026, 11, 1)));
        assert_eq!(dump_date("reports_sep3_2019.tar.gz"), Some((2019, 9, 3)));
        assert_eq!(dump_date("reports_aug4_2026.tar.gz"), Some((2026, 8, 4)));
        assert_eq!(dump_date("README.md"), None);
        assert_eq!(dump_date("reports_foo1_2026.tar.gz"), None);
        assert!(dump_date("reports_jan2_2027.tar.gz") > dump_date("reports_dec1_2026.tar.gz"));
    }
}
