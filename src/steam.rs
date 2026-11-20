//! Steam store metadata: tags (genres), native Linux support and Steam Deck
//! status. Uses the keyless endpoints the Steam store frontend itself calls.

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::http::send_with_retry;

/// How many apps to ask for per request. Steam accepts at least 250.
pub const BATCH_SIZE: usize = 200;

#[derive(Debug, Clone)]
pub struct StoreItem {
    pub appid: u32,
    /// False when Steam returned nothing for the id (removed, region locked, bad id).
    pub found: bool,
    pub name: Option<String>,
    pub native: bool,
    /// 0 unknown, 1 unsupported, 2 playable, 3 verified.
    pub deck: i64,
    /// Tag ids, most-voted first.
    pub tagids: Vec<u32>,
}

#[derive(Deserialize)]
struct TagListResponse {
    response: TagListInner,
}

#[derive(Deserialize)]
struct TagListInner {
    #[serde(default)]
    tags: Vec<RawTag>,
}

#[derive(Deserialize)]
struct RawTag {
    tagid: u32,
    name: String,
}

#[derive(Deserialize)]
struct ItemsResponse {
    response: ItemsInner,
}

#[derive(Deserialize)]
struct ItemsInner {
    #[serde(default)]
    store_items: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawItem {
    id: u32,
    #[serde(default)]
    success: i64,
    name: Option<String>,
    #[serde(default)]
    tagids: Vec<u32>,
    platforms: Option<RawPlatforms>,
}

#[derive(Deserialize)]
struct RawPlatforms {
    #[serde(default)]
    steamos_linux: bool,
    #[serde(default)]
    steam_deck_compat_category: i64,
}

/// All Steam tags with their English names.
pub async fn fetch_tags(client: &Client) -> Result<Vec<(u32, String)>> {
    let url = "https://api.steampowered.com/IStoreService/GetTagList/v1/?language=english";
    let resp = send_with_retry(|| client.get(url)).await?;
    if !resp.status().is_success() {
        bail!("Steam tag list returned HTTP {}", resp.status());
    }
    let body: TagListResponse = resp.json().await.context("parsing Steam tag list")?;
    Ok(body
        .response
        .tags
        .into_iter()
        .map(|t| (t.tagid, t.name))
        .collect())
}

/// Store data for a batch of apps. Every requested id gets an entry back, with
/// `found = false` if Steam didn't return it.
pub async fn fetch_items(client: &Client, appids: &[u32]) -> Result<Vec<StoreItem>> {
    let input = json!({
        "ids": appids.iter().map(|id| json!({ "appid": id })).collect::<Vec<_>>(),
        "context": { "language": "english", "country_code": "US" },
        "data_request": { "include_platforms": true, "include_tag_count": 20 },
    })
    .to_string();
    let url = "https://api.steampowered.com/IStoreBrowseService/GetItems/v1/";
    let resp = send_with_retry(|| client.get(url).query(&[("input_json", &input)])).await?;
    if !resp.status().is_success() {
        bail!("Steam store returned HTTP {}", resp.status());
    }
    let body: ItemsResponse = resp.json().await.context("parsing Steam store items")?;

    let mut by_id: std::collections::HashMap<u32, RawItem> = body
        .response
        .store_items
        .into_iter()
        .map(|item| (item.id, item))
        .collect();

    Ok(appids
        .iter()
        .map(|&appid| match by_id.remove(&appid) {
            Some(item) if item.success == 1 => {
                let platforms = item.platforms;
                StoreItem {
                    appid,
                    found: true,
                    name: item.name.filter(|n| !n.trim().is_empty()),
                    native: platforms.as_ref().is_some_and(|p| p.steamos_linux),
                    deck: platforms.map_or(0, |p| p.steam_deck_compat_category),
                    tagids: item.tagids,
                }
            }
            _ => StoreItem {
                appid,
                found: false,
                name: None,
                native: false,
                deck: 0,
                tagids: Vec::new(),
            },
        })
        .collect())
}
