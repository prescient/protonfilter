use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::http::{Pacer, send_with_retry};

/// ProtonDB answers 403 (sometimes 429) for about a minute once a client goes
/// past roughly 1000 requests a minute.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(45);
const RATE_LIMIT_RETRIES: u32 = 6;

/// The per-game summary ProtonDB's own site uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub tier: Option<String>,
    pub trending_tier: Option<String>,
    pub best_reported_tier: Option<String>,
    pub score: Option<f64>,
    pub confidence: Option<String>,
    pub total: Option<u32>,
}

pub fn game_url(appid: u32) -> String {
    format!("https://www.protondb.com/app/{appid}")
}

/// Fetches the summary for one app. Returns `Ok(None)` when ProtonDB has no
/// summary for it (the endpoint answers 404 with an HTML page).
pub async fn fetch_summary(client: &Client, pacer: &Pacer, appid: u32) -> Result<Option<Summary>> {
    let url = format!("https://www.protondb.com/api/v1/reports/summaries/{appid}.json");
    let mut limited = 0;
    loop {
        pacer.wait().await;
        let resp = send_with_retry(|| client.get(&url)).await?;
        match resp.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
                if limited < RATE_LIMIT_RETRIES =>
            {
                limited += 1;
                pacer.pause(RATE_LIMIT_PAUSE);
            }
            s if s.is_success() => {
                let summary = resp
                    .json::<Summary>()
                    .await
                    .with_context(|| format!("parsing ProtonDB summary for app {appid}"))?;
                return Ok(Some(summary));
            }
            s => bail!("ProtonDB returned HTTP {s} for app {appid}"),
        }
    }
}
