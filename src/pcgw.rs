//! PCGamingWiki: which graphics APIs (DirectX versions, Vulkan, OpenGL) each
//! game's Windows version uses. The wiki's bulk query API is closed to
//! anonymous clients, so this reads the game pages themselves through the
//! MediaWiki API, up to 50 per request. A game's page is found by its Steam
//! name, or failing that by asking the wiki which page has its app id.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, header::LOCATION, redirect};
use serde::Deserialize;
use serde_json::Value;

use crate::http::{self, Pacer, send_with_retry};
use crate::model::GraphicsApis;

/// Titles per request; the most MediaWiki allows when fetching page text.
pub const BATCH_SIZE: usize = 50;
const API_URL: &str = "https://www.pcgamingwiki.com/w/api.php";
const APPID_URL: &str = "https://www.pcgamingwiki.com/api/appid.php";
const WIKI_URL: &str = "https://www.pcgamingwiki.com/wiki/";

/// A client that doesn't follow redirects: the app id lookup answers with
/// one, and its target is the page title we want.
pub fn client() -> Result<Client> {
    http::builder()
        .redirect(redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    /// Steam app ids the page says it covers.
    pub steam_appids: Vec<u32>,
    /// `None` when the page doesn't list any graphics APIs.
    pub apis: Option<GraphicsApis>,
}

pub fn page_url(title: &str) -> String {
    let mut url = String::from(WIKI_URL);
    for b in title.replace(' ', "_").bytes() {
        if b.is_ascii_alphanumeric() || b"-._~;@$!*(),/:'".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// A game to look up by its Steam name, or by the page found for it last time.
#[derive(Debug, Clone)]
pub struct NameQuery {
    pub appid: u32,
    pub name: String,
    pub known_page: Option<String>,
}

impl NameQuery {
    /// Titles to try, best first, and whether a hit can be trusted without
    /// checking that the page lists this app id.
    fn candidates(&self) -> Vec<(String, bool)> {
        match &self.known_page {
            Some(page) => vec![(page.clone(), true)],
            None => titles_for_name(&self.name)
                .into_iter()
                .map(|t| (t, false))
                .collect(),
        }
    }
}

/// Splits name lookups into groups that each fit in one request.
pub fn name_batches(queries: Vec<NameQuery>) -> Vec<Vec<NameQuery>> {
    let mut batches = Vec::new();
    let (mut batch, mut titles) = (Vec::new(), 0);
    for q in queries {
        let n = q.candidates().len();
        if titles + n > BATCH_SIZE && !batch.is_empty() {
            batches.push(std::mem::take(&mut batch));
            titles = 0;
        }
        titles += n;
        batch.push(q);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// Looks up each game under its known page or Steam name. A page found by
/// name only counts if it lists the game's app id, so two games that share
/// a name don't get each other's page.
pub async fn find_by_name(
    client: &Client,
    pacer: &Pacer,
    queries: &[NameQuery],
) -> Result<Vec<(u32, Option<Page>)>> {
    let mut titles: Vec<String> = queries
        .iter()
        .flat_map(|q| q.candidates().into_iter().map(|(t, _)| t))
        .collect();
    titles.sort();
    titles.dedup();
    let pages = fetch_pages(client, pacer, &titles).await?;
    Ok(queries
        .iter()
        .map(|q| {
            let page = q.candidates().into_iter().find_map(|(title, trusted)| {
                pages
                    .get(&title)
                    .filter(|p| trusted || p.steam_appids.contains(&q.appid))
                    .cloned()
            });
            (q.appid, page)
        })
        .collect())
}

/// Asks the wiki which page covers each app id, then fetches those pages.
pub async fn find_by_appid(
    client: &Client,
    pacer: &Pacer,
    appids: &[u32],
) -> Result<Vec<(u32, Option<Page>)>> {
    let mut titles = Vec::with_capacity(appids.len());
    for &appid in appids {
        titles.push((appid, title_for_appid(client, pacer, appid).await?));
    }
    let wanted: Vec<String> = titles.iter().filter_map(|(_, t)| t.clone()).collect();
    let pages = fetch_pages(client, pacer, &wanted).await?;
    Ok(titles
        .into_iter()
        .map(|(appid, title)| (appid, title.and_then(|t| pages.get(&t).cloned())))
        .collect())
}

/// The title of the wiki's page for a Steam app, if it has one.
async fn title_for_appid(client: &Client, pacer: &Pacer, appid: u32) -> Result<Option<String>> {
    pacer.wait().await;
    let resp = send_with_retry(|| client.get(APPID_URL).query(&[("appid", appid)])).await?;
    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        return Ok(title_from_url(location));
    }
    check_status(status)?;
    // Unknown app ids get an ordinary page rather than a redirect.
    Ok(None)
}

/// Fetches pages by title, following redirects. Returns each requested
/// title that exists, mapped to its page.
pub async fn fetch_pages(
    client: &Client,
    pacer: &Pacer,
    titles: &[String],
) -> Result<HashMap<String, Page>> {
    let mut found = Found::default();
    if titles.is_empty() {
        return Ok(HashMap::new());
    }
    let joined = titles.join("|");
    let mut continuation: Vec<(String, String)> = Vec::new();
    // Very long pages can push the rest of a batch into a follow-up request.
    for _ in 0..BATCH_SIZE {
        let mut form: Vec<(String, String)> = [
            ("action", "query"),
            ("format", "json"),
            ("formatversion", "2"),
            ("prop", "revisions"),
            ("rvprop", "content"),
            ("rvslots", "main"),
            ("redirects", "1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        form.push(("titles".into(), joined.clone()));
        form.extend(continuation.iter().cloned());

        pacer.wait().await;
        let resp = send_with_retry(|| client.post(API_URL).form(&form)).await?;
        check_status(resp.status())?;
        let body: QueryResponse = resp.json().await.context("parsing PCGamingWiki response")?;
        match found.absorb(body)? {
            Some(next) => continuation = next,
            None => break,
        }
    }
    Ok(found.resolve(titles))
}

fn check_status(status: StatusCode) -> Result<()> {
    if status == StatusCode::FORBIDDEN {
        bail!("PCGamingWiki refused the request (HTTP 403); try again later");
    }
    if !status.is_success() {
        bail!("PCGamingWiki returned HTTP {status}");
    }
    Ok(())
}

/// Page text and title redirects collected across a batch's responses.
#[derive(Default)]
struct Found {
    /// Requested spelling or redirect -> where it leads.
    aliases: HashMap<String, String>,
    texts: HashMap<String, String>,
}

impl Found {
    /// Takes in one response; returns the parameters for the next request if
    /// the server didn't send everything.
    fn absorb(&mut self, body: QueryResponse) -> Result<Option<Vec<(String, String)>>> {
        if let Some(e) = body.error {
            bail!("PCGamingWiki API error: {} ({})", e.info, e.code);
        }
        if let Some(q) = body.query {
            for alias in q.normalized.into_iter().chain(q.redirects) {
                self.aliases.insert(alias.from, alias.to);
            }
            for page in q.pages {
                let text = page
                    .revisions
                    .into_iter()
                    .next()
                    .and_then(|r| r.slots.main.content);
                if let (Some(title), Some(text)) = (page.title, text) {
                    self.texts.insert(title, text);
                }
            }
        }
        Ok(body.continuation.map(|c| {
            c.into_iter()
                .map(|(k, v)| match v {
                    Value::String(s) => (k, s),
                    other => (k, other.to_string()),
                })
                .collect()
        }))
    }

    fn resolve(&self, titles: &[String]) -> HashMap<String, Page> {
        titles
            .iter()
            .filter_map(|requested| {
                let mut title = requested;
                // Normalised spelling, then up to a couple of redirects.
                for _ in 0..4 {
                    match self.aliases.get(title) {
                        Some(next) if next != title => title = next,
                        _ => break,
                    }
                }
                let text = self.texts.get(title)?;
                Some((requested.clone(), parse_page(title, text)))
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct QueryResponse {
    query: Option<Query>,
    #[serde(rename = "continue")]
    continuation: Option<HashMap<String, Value>>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    #[serde(default)]
    info: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Query {
    normalized: Vec<Alias>,
    redirects: Vec<Alias>,
    pages: Vec<RawPage>,
}

#[derive(Deserialize)]
struct Alias {
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct RawPage {
    title: Option<String>,
    #[serde(default)]
    revisions: Vec<Revision>,
}

#[derive(Deserialize)]
struct Revision {
    slots: Slots,
}

#[derive(Deserialize)]
struct Slots {
    main: Slot,
}

#[derive(Deserialize)]
struct Slot {
    content: Option<String>,
}

/// Reads the Steam app ids and graphics APIs out of a page's wikitext.
pub fn parse_page(title: &str, text: &str) -> Page {
    let mut steam_appids = Vec::new();
    for key in ["steam appid", "steam appid side"] {
        if let Some(v) = field(text, key) {
            steam_appids.extend(
                v.split(|c: char| c == ',' || c.is_whitespace())
                    .filter_map(|s| s.trim().parse::<u32>().ok()),
            );
        }
    }
    let apis = GraphicsApis {
        directx: field(text, "direct3d versions")
            .map(directx_versions)
            .unwrap_or_default(),
        vulkan: field(text, "vulkan versions").is_some_and(is_supported),
        opengl: field(text, "opengl versions").is_some_and(is_supported),
    };
    Page {
        title: title.to_string(),
        steam_appids,
        apis: (!apis.is_empty()).then_some(apis),
    }
}

/// The value of the first `|key = value` template parameter, cut at any
/// reference or nested template.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = text;
    while let Some(i) = rest.find('|') {
        rest = &rest[i + 1..];
        let Some(after_key) = rest.strip_prefix(key) else {
            continue;
        };
        let Some(value) = after_key.trim_start_matches([' ', '\t']).strip_prefix('=') else {
            continue;
        };
        let mut value = &value[..value.find(['\n', '|']).unwrap_or(value.len())];
        for stop in ["<ref", "{{", "<!--"] {
            if let Some(j) = value.find(stop) {
                value = &value[..j];
            }
        }
        return Some(value.trim());
    }
    None
}

/// `"9.0c, 11"` -> `[9, 11]`.
fn directx_versions(value: &str) -> Vec<u8> {
    let mut versions: Vec<u8> = value
        .split([',', '/', ';'])
        .filter_map(|part| {
            let digits: String = part
                .trim()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok().filter(|v| (1..=12).contains(v))
        })
        .collect();
    versions.sort_unstable();
    versions.dedup();
    versions
}

/// Vulkan and OpenGL fields hold a version, `true`, or `false`/`unknown`.
fn is_supported(value: &str) -> bool {
    let v = value.trim();
    v.eq_ignore_ascii_case("true") || v.starts_with(|c: char| c.is_ascii_digit())
}

/// Page titles to try for a Steam name, best guess first. Wiki titles mostly
/// match Steam names without trademark signs, and all-caps names
/// ("ELDEN RING") are usually title-cased there.
fn titles_for_name(name: &str) -> Vec<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, '™' | '®' | '©'))
        .map(|c| match c {
            '’' | '‘' => '\'',
            '“' | '”' => '"',
            c => c,
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if !valid_title(&cleaned) {
        return Vec::new();
    }
    let mut titles = vec![cleaned.clone()];
    let letters: Vec<char> = cleaned.chars().filter(|c| c.is_alphabetic()).collect();
    let shouting = letters.len() > 3
        && letters.iter().any(|c| c.is_uppercase())
        && !letters.iter().any(|c| c.is_lowercase());
    if shouting {
        let titled = cleaned
            .split(' ')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first
                        .to_uppercase()
                        .chain(chars.flat_map(char::to_lowercase))
                        .collect(),
                    None => String::new(),
                }
            })
            .collect::<Vec<String>>()
            .join(" ");
        titles.push(titled);
    }
    titles
}

/// Characters MediaWiki doesn't allow in titles (`|` also separates titles
/// in a request).
fn valid_title(title: &str) -> bool {
    !title.is_empty()
        && title.len() <= 255
        && !title
            .chars()
            .any(|c| "#<>[]|{}".contains(c) || c.is_control())
}

/// `https://www.pcgamingwiki.com/wiki/The_Witcher_3:_Wild_Hunt` -> `The Witcher 3: Wild Hunt`.
fn title_from_url(url: &str) -> Option<String> {
    let path = url.split_once("/wiki/")?.1;
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'_', _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    let title = String::from_utf8(out).ok()?;
    (!title.trim().is_empty()).then(|| title.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITCHER: &str = "{{Infobox game
|cover        = The Witcher 3 cover.jpg
|steam appid  = 292030
|steam appid side = 355880, 378648
|gogcom id    = 1207664643
}}
{{API
|direct3d versions      = 12
|direct3d notes         = Direct3D 11 support was removed.<ref>{{Refurl|url=x}}</ref>
|directdraw versions    =
|opengl versions        =
|vulkan versions        =
|shader model versions  = 5
}}";

    #[test]
    fn reads_app_ids_and_apis() {
        let page = parse_page("The Witcher 3: Wild Hunt", WITCHER);
        assert_eq!(page.steam_appids, vec![292030, 355880, 378648]);
        let apis = page.apis.unwrap();
        assert_eq!(apis.directx, vec![12]);
        assert!(apis.needs_dx12());
    }

    #[test]
    fn api_values_in_their_various_spellings() {
        let page = parse_page(
            "x",
            "|steam appid = 1\n|direct3d versions = 9.0c, 11.1, 11\n|vulkan versions = 1.3<ref>{{Refurl|url=y}}</ref>\n|opengl versions = false\n",
        );
        let apis = page.apis.unwrap();
        assert_eq!(apis.directx, vec![9, 11]);
        assert!(apis.vulkan);
        assert!(!apis.opengl);

        let gl = parse_page("y", "|opengl versions = true\n|vulkan versions = unknown\n");
        assert_eq!(gl.apis.unwrap().to_tokens(), "opengl");

        assert_eq!(parse_page("z", "|direct3d versions =\n").apis, None);
        assert!(parse_page("z", "no templates").steam_appids.is_empty());
    }

    #[test]
    fn steam_names_to_wiki_titles() {
        assert_eq!(titles_for_name("Hades"), vec!["Hades"]);
        assert_eq!(
            titles_for_name("ELDEN RING"),
            vec!["ELDEN RING", "Elden Ring"]
        );
        assert_eq!(
            titles_for_name("Tom Clancy’s Rainbow Six®  Siege"),
            vec!["Tom Clancy's Rainbow Six Siege"]
        );
        assert_eq!(titles_for_name("DOOM"), vec!["DOOM", "Doom"]);
        assert_eq!(
            titles_for_name("FTL"),
            vec!["FTL"],
            "short names stay as-is"
        );
        assert!(titles_for_name("Game [Beta]").is_empty());
        assert!(titles_for_name("A | B").is_empty());
    }

    #[test]
    fn known_pages_are_trusted_and_batches_stay_under_the_limit() {
        let q = |appid, name: &str, known: Option<&str>| NameQuery {
            appid,
            name: name.into(),
            known_page: known.map(Into::into),
        };
        assert_eq!(
            q(1, "ELDEN RING", Some("Elden Ring")).candidates(),
            vec![("Elden Ring".to_string(), true)]
        );
        let queries: Vec<NameQuery> = (0..60).map(|i| q(i, "LOUD NAME", None)).collect();
        let batches = name_batches(queries);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![25, 25, 10]
        );
    }

    #[test]
    fn follows_normalised_titles_and_redirects() {
        let body: QueryResponse = serde_json::from_str(
            r#"{"continue": {"rvcontinue": "123|456", "continue": "||"},
                "query": {
                  "normalized": [{"from": "witcher 3", "to": "Witcher 3"}],
                  "redirects": [{"from": "Witcher 3", "to": "The Witcher 3: Wild Hunt"}],
                  "pages": [
                    {"title": "The Witcher 3: Wild Hunt", "revisions": [{"slots": {"main": {"content": "|steam appid = 292030\n|direct3d versions = 12\n"}}}]},
                    {"title": "Nope", "missing": true},
                    {"title": "Bad [x]", "invalid": true}
                  ]}}"#,
        )
        .unwrap();
        let mut found = Found::default();
        let next = found.absorb(body).unwrap().unwrap();
        assert!(next.contains(&("rvcontinue".to_string(), "123|456".to_string())));

        let titles = ["witcher 3", "Nope", "Bad [x]"].map(String::from);
        let pages = found.resolve(&titles);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages["witcher 3"].title, "The Witcher 3: Wild Hunt");
        assert_eq!(pages["witcher 3"].steam_appids, vec![292030]);
    }

    #[test]
    fn api_errors_are_reported() {
        let body: QueryResponse =
            serde_json::from_str(r#"{"error": {"code": "ratelimited", "info": "Slow down"}}"#)
                .unwrap();
        let err = Found::default().absorb(body).unwrap_err().to_string();
        assert!(err.contains("Slow down"), "{err}");
    }

    #[test]
    fn urls_and_titles() {
        assert_eq!(
            page_url("The Witcher 3: Wild Hunt"),
            "https://www.pcgamingwiki.com/wiki/The_Witcher_3:_Wild_Hunt"
        );
        assert_eq!(
            page_url("Tom Clancy's Rainbow Six Siege?"),
            "https://www.pcgamingwiki.com/wiki/Tom_Clancy's_Rainbow_Six_Siege%3F"
        );
        assert_eq!(
            title_from_url("https://www.pcgamingwiki.com/wiki/The_Witcher_3:_Wild_Hunt"),
            Some("The Witcher 3: Wild Hunt".into())
        );
        assert_eq!(
            title_from_url("https://www.pcgamingwiki.com/wiki/Pok%C3%A9mon_Legends#Top"),
            Some("Pokémon Legends".into())
        );
        assert_eq!(title_from_url("https://www.pcgamingwiki.com/"), None);
    }
}
