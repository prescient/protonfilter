use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, RequestBuilder, Response, StatusCode};

const MAX_ATTEMPTS: u32 = 4;

pub fn client() -> Result<Client> {
    builder().build().context("building HTTP client")
}

pub fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .user_agent(concat!("protonfilter/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
}

/// Sends a request, retrying on connection errors, 429 and 5xx with
/// exponential backoff (honouring `Retry-After` when present). Other statuses,
/// including 404, are returned to the caller unchanged.
pub async fn send_with_retry(make: impl Fn() -> RequestBuilder) -> Result<Response> {
    let mut delay = Duration::from_secs(1);
    let mut last_err = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match make().send().await {
            Ok(resp) => {
                let status = resp.status();
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                if !retryable {
                    return Ok(resp);
                }
                let wait = retry_after(&resp).unwrap_or(delay);
                last_err = Some(anyhow!("HTTP {status} from {}", resp.url()));
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(wait).await;
                }
            }
            Err(e) => {
                last_err = Some(anyhow::Error::new(e));
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                }
            }
        }
        delay *= 2;
    }
    Err(last_err.unwrap_or_else(|| anyhow!("request failed")))
}

fn retry_after(resp: &Response) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(120)))
}

/// Spaces requests out to a steady rate shared by all workers, and lets any
/// worker pause everyone when the server says to slow down.
pub struct Pacer {
    interval: Duration,
    next: std::sync::Mutex<std::time::Instant>,
}

impl Pacer {
    pub fn per_second(rate: f64) -> Self {
        Pacer {
            interval: Duration::from_secs_f64(1.0 / rate.max(0.1)),
            next: std::sync::Mutex::new(std::time::Instant::now()),
        }
    }

    /// Waits for this caller's turn.
    pub async fn wait(&self) {
        let slot = {
            let mut next = self.next.lock().expect("pacer lock");
            let slot = (*next).max(std::time::Instant::now());
            *next = slot + self.interval;
            slot
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(slot)).await;
    }

    /// Holds back every caller for at least `d` from now.
    pub fn pause(&self, d: Duration) {
        let mut next = self.next.lock().expect("pacer lock");
        let until = std::time::Instant::now() + d;
        if *next < until {
            *next = until;
        }
    }
}
