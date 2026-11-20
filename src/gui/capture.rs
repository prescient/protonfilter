//! Debug builds only: open the window with a given state, save a PNG of it
//! once results are on screen, and exit. Lets the UI be checked without
//! clicking through it by hand.
//!
//! ```sh
//! PROTONFILTER_CAPTURE=/tmp/shot.png \
//! PROTONFILTER_CAPTURE_STATE='{"filters": {"tiers": ["gold"]}, "select": 1245620}' \
//! cargo run -- gui
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;
use serde::Deserialize;

use super::Filters;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct CaptureState {
    pub filters: Option<Filters>,
    pub select: Option<u32>,
    pub tag_query: String,
    pub dark: Option<bool>,
    /// Run a sync first and capture once it finishes, with the log open.
    pub sync: bool,
    /// Refresh the selected game first.
    pub refresh: bool,
    pub width: Option<f32>,
    pub height: Option<f32>,
}

pub struct Capture {
    pub path: PathBuf,
    pub state: CaptureState,
    pub jobs_started: bool,
    settled_frames: u32,
    requested: bool,
    done: Arc<AtomicBool>,
}

impl Capture {
    pub fn from_env() -> Option<Capture> {
        if !cfg!(debug_assertions) {
            return None;
        }
        let path = PathBuf::from(std::env::var_os("PROTONFILTER_CAPTURE")?);
        let state = match std::env::var("PROTONFILTER_CAPTURE_STATE") {
            Ok(json) => {
                serde_json::from_str(&json).expect("PROTONFILTER_CAPTURE_STATE must be valid JSON")
            }
            Err(_) => CaptureState::default(),
        };
        Some(Capture {
            path,
            state,
            jobs_started: false,
            settled_frames: 0,
            requested: false,
            done: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Call every frame; `ready` means results for the current filters are shown.
    pub fn frame(&mut self, ctx: &egui::Context, ready: bool) {
        if self.done.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        ctx.request_repaint();
        if !ready || self.requested {
            return;
        }
        // Give column widths and fonts a few frames to settle.
        self.settled_frames += 1;
        if self.settled_frames < 10 {
            return;
        }
        self.requested = true;
        let (path, done, ctx2) = (self.path.clone(), self.done.clone(), ctx.clone());
        ctx.request_screenshot(move |image| {
            match write_png(&path, &image) {
                Ok(()) => eprintln!("saved {}", path.display()),
                Err(e) => eprintln!("couldn't save {}: {e}", path.display()),
            }
            done.store(true, Ordering::Relaxed);
            ctx2.request_repaint();
        });
    }
}

fn write_png(path: &std::path::Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let [width, height] = image.size;
    let mut raw = Vec::with_capacity((width * 4 + 1) * height);
    for row in image.pixels.chunks(width) {
        raw.push(0); // no filter
        for pixel in row {
            raw.extend_from_slice(&pixel.to_srgba_unmultiplied());
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw)?;
    let idat = z.finish()?;

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [
        (b"IHDR", &ihdr[..]),
        (b"IDAT", &idat[..]),
        (b"IEND", &[][..]),
    ] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    std::fs::write(path, out)
}
