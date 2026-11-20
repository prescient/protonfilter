use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Compatibility rating. Declaration order is the ranking used by `--min-tier`
/// and `--sort tier`: a native Linux build beats any Proton rating.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Pending,
    Borked,
    Bronze,
    Silver,
    Gold,
    Platinum,
    Native,
}

impl Tier {
    /// Tiers that ProtonDB itself assigns (everything except `Native`).
    pub const PROTON: [Tier; 6] = [
        Tier::Pending,
        Tier::Borked,
        Tier::Bronze,
        Tier::Silver,
        Tier::Gold,
        Tier::Platinum,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Pending => "pending",
            Tier::Borked => "borked",
            Tier::Bronze => "bronze",
            Tier::Silver => "silver",
            Tier::Gold => "gold",
            Tier::Platinum => "platinum",
            Tier::Native => "native",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Some(match s.to_ascii_lowercase().as_str() {
            "pending" => Tier::Pending,
            "borked" => Tier::Borked,
            "bronze" => Tier::Bronze,
            "silver" => Tier::Silver,
            "gold" => Tier::Gold,
            "platinum" => Tier::Platinum,
            "native" => Tier::Native,
            _ => return None,
        })
    }
}

/// Which ProtonDB tier field to filter and sort on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
pub enum TierSource {
    /// The tier ProtonDB shows on the game page.
    #[default]
    Current,
    /// The tier from recent reports only.
    Trending,
    /// The best tier anyone has reported.
    Best,
}

impl TierSource {
    pub fn column(self) -> &'static str {
        match self {
            TierSource::Current => "tier",
            TierSource::Trending => "trending_tier",
            TierSource::Best => "best_tier",
        }
    }
}

/// Steam Deck compatibility, as reported by the Steam store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deck {
    Unknown,
    Unsupported,
    Playable,
    Verified,
}

impl Deck {
    pub fn from_code(code: i64) -> Deck {
        match code {
            1 => Deck::Unsupported,
            2 => Deck::Playable,
            3 => Deck::Verified,
            _ => Deck::Unknown,
        }
    }

    pub fn code(self) -> i64 {
        match self {
            Deck::Unknown => 0,
            Deck::Unsupported => 1,
            Deck::Playable => 2,
            Deck::Verified => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Deck::Unknown => "unknown",
            Deck::Unsupported => "unsupported",
            Deck::Playable => "playable",
            Deck::Verified => "verified",
        }
    }
}

/// The graphics APIs a game's Windows version can use, from PCGamingWiki.
/// Under Proton, Direct3D 9-11 run through DXVK and Direct3D 12 through
/// VKD3D-Proton.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct GraphicsApis {
    /// Direct3D major versions, lowest first.
    pub directx: Vec<u8>,
    pub vulkan: bool,
    pub opengl: bool,
}

impl GraphicsApis {
    pub fn is_empty(&self) -> bool {
        self.directx.is_empty() && !self.vulkan && !self.opengl
    }

    /// DirectX 12 is the only option, so there's nothing to switch to.
    pub fn needs_dx12(&self) -> bool {
        self.directx == [12] && !self.vulkan && !self.opengl
    }

    /// Stored form, e.g. `dx11,dx12,vulkan`. The database filters on it, so
    /// the order is fixed: `dx12` alone means DX12 is required.
    pub fn to_tokens(&self) -> String {
        let mut tokens: Vec<String> = self.directx.iter().map(|v| format!("dx{v}")).collect();
        if self.opengl {
            tokens.push("opengl".into());
        }
        if self.vulkan {
            tokens.push("vulkan".into());
        }
        tokens.join(",")
    }

    pub fn from_tokens(s: &str) -> GraphicsApis {
        let mut apis = GraphicsApis::default();
        for token in s.split(',').map(str::trim) {
            match token {
                "vulkan" => apis.vulkan = true,
                "opengl" => apis.opengl = true,
                t => {
                    if let Some(v) = t.strip_prefix("dx").and_then(|v| v.parse().ok()) {
                        apis.directx.push(v);
                    }
                }
            }
        }
        apis.directx.sort_unstable();
        apis.directx.dedup();
        apis
    }

    /// Short form for tables: `DX11/12 + VK`.
    pub fn short_label(&self) -> String {
        let mut parts = Vec::new();
        if !self.directx.is_empty() {
            let versions: Vec<String> = self.directx.iter().map(u8::to_string).collect();
            parts.push(format!("DX{}", versions.join("/")));
        }
        if self.vulkan {
            parts.push("VK".into());
        }
        if self.opengl {
            parts.push("GL".into());
        }
        parts.join(" + ")
    }

    /// Long form: `DirectX 11, DirectX 12, Vulkan`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = self
            .directx
            .iter()
            .map(|v| format!("DirectX {v}"))
            .collect();
        if self.vulkan {
            parts.push("Vulkan".into());
        }
        if self.opengl {
            parts.push("OpenGL".into());
        }
        parts.join(", ")
    }
}

/// Filtering on DirectX 12, which runs through VKD3D-Proton under Proton.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
pub enum Dx12Filter {
    /// Don't filter on DirectX 12.
    #[default]
    Any,
    /// Hide games that need DirectX 12 (no DirectX 11, Vulkan or OpenGL to use instead).
    NotRequired,
    /// Hide every game that supports DirectX 12, even with another option.
    Exclude,
}

/// How far the PCGamingWiki lookup for a game has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WikiLookup {
    /// Not looked up yet.
    #[default]
    Pending,
    /// No page under the game's Steam name; the app id lookup hasn't run.
    NameMissed,
    /// No page under its name or its app id.
    NotFound,
    /// Has a page, though the page may not list graphics APIs.
    Found,
}

/// Steam's top-level store genres. `--genre` accepts any Steam tag, but these
/// are what `protonfilter genres` lists by default.
pub const STEAM_GENRES: [&str; 12] = [
    "Action",
    "Adventure",
    "Casual",
    "Early Access",
    "Free to Play",
    "Indie",
    "Massively Multiplayer",
    "Racing",
    "RPG",
    "Simulation",
    "Sports",
    "Strategy",
];

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub appid: u32,
    pub name: String,
    pub tier: Option<String>,
    pub trending_tier: Option<String>,
    pub best_tier: Option<String>,
    pub score: Option<f64>,
    pub confidence: Option<String>,
    /// `None` when Steam has no store data for the app (delisted or region locked).
    pub native: Option<bool>,
    pub deck: Option<Deck>,
    /// Report count from the live ProtonDB summary, or the dump count if no summary yet.
    pub reports: u32,
    /// Reports with written notes, counted from the monthly ProtonDB dump.
    pub comments: u32,
    pub last_report_at: Option<i64>,
    pub tags: Vec<String>,
    /// From PCGamingWiki; `None` when unknown.
    pub graphics_apis: Option<GraphicsApis>,
    pub protondb_url: String,
    pub pcgamingwiki_url: Option<String>,
    #[serde(skip)]
    pub wiki_lookup: WikiLookup,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds to `YYYY-MM-DD` (UTC), without pulling in a date crate.
pub fn format_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ordering_puts_native_on_top() {
        assert!(Tier::Native > Tier::Platinum);
        assert!(Tier::Platinum > Tier::Gold);
        assert!(Tier::Borked > Tier::Pending);
    }

    #[test]
    fn graphics_api_tokens_and_labels() {
        let apis = GraphicsApis::from_tokens("vulkan,dx12,dx11");
        assert_eq!(apis.directx, vec![11, 12]);
        assert_eq!(apis.to_tokens(), "dx11,dx12,vulkan");
        assert_eq!(apis.short_label(), "DX11/12 + VK");
        assert_eq!(apis.label(), "DirectX 11, DirectX 12, Vulkan");
        assert!(!apis.needs_dx12(), "DX11 is there to fall back on");

        let only12 = GraphicsApis::from_tokens("dx12");
        assert!(only12.needs_dx12());
        assert_eq!(only12.to_tokens(), "dx12");

        let gl = GraphicsApis {
            directx: vec![],
            vulkan: false,
            opengl: true,
        };
        assert_eq!(GraphicsApis::from_tokens(&gl.to_tokens()), gl);
        assert!(GraphicsApis::from_tokens("").is_empty());
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_572_299_227), "2019-10-28");
        assert_eq!(format_date(1_794_796_234), "2026-11-16");
        assert_eq!(format_date(951_782_400), "2000-02-29");
    }
}
