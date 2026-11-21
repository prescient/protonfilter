//! Desktop window: filters on the left, results in the middle, details for
//! the selected game on the right. Queries run on a worker thread so typing
//! and dragging sliders never wait on SQLite.

mod capture;

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use eframe::egui::{self, Color32, OpenUrl, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};

use crate::db::{Db, Filter, Order, SortKey, Status};
use crate::model::{
    Deck, Dx12Filter, Game, GraphicsApis, STEAM_GENRES, Tier, TierSource, WikiLookup, format_date,
};
use crate::sync::{SyncOptions, SyncProgress, Unit};
use crate::{http, sync};

const STORAGE_KEY: &str = "filters";
/// `top_tags` at this value means "match any of a game's tags".
const ALL_TAGS: u32 = 20;
const TIERS: [Tier; 7] = [
    Tier::Native,
    Tier::Platinum,
    Tier::Gold,
    Tier::Silver,
    Tier::Bronze,
    Tier::Borked,
    Tier::Pending,
];
const DECKS: [Deck; 4] = [
    Deck::Verified,
    Deck::Playable,
    Deck::Unsupported,
    Deck::Unknown,
];

pub fn run(db_path: PathBuf, cache_dir: PathBuf) -> Result<()> {
    // Surface a bad --db path in the terminal rather than in an empty window.
    Db::open(&db_path)?;
    let capture = capture::Capture::from_env();
    let size = capture
        .as_ref()
        .map(|c| {
            [
                c.state.width.unwrap_or(1360.0),
                c.state.height.unwrap_or(820.0),
            ]
        })
        .unwrap_or([1360.0, 820.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ProtonFilter")
        .with_app_id("protonfilter")
        .with_inner_size(size)
        .with_min_inner_size([900.0, 520.0]);
    // Rendered from assets/protonfilter.svg. On Wayland, desktops take the
    // taskbar icon from the .desktop file matching the app id instead.
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../../assets/protonfilter-256.png"))
    {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    if let Some(c) = &capture {
        // Keep capture runs away from the user's saved window and filters.
        options.persist_window = false;
        options.persistence_path = Some(c.path.with_extension("egui-state"));
    }
    eframe::run_native(
        "protonfilter",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, db_path, cache_dir, capture)))),
    )
    .map_err(|e| anyhow!("couldn't open the window: {e}"))
}

// ---------------------------------------------------------------------------
// Filter state (remembered between runs)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Filters {
    name: String,
    include: Vec<String>,
    exclude: Vec<String>,
    any_genre: bool,
    top_tags: u32,
    tiers: BTreeSet<Tier>,
    tier_source: TierSource,
    proton_only: bool,
    dx12: Dx12Filter,
    hide_unknown_api: bool,
    vulkan: bool,
    min_reports: u32,
    max_reports: Option<u32>,
    min_comments: u32,
    max_comments: Option<u32>,
    deck: Vec<Deck>,
    sort: SortKey,
    order: Option<Order>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            name: String::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            any_genre: false,
            top_tags: ALL_TAGS,
            tiers: BTreeSet::new(),
            tier_source: TierSource::Current,
            proton_only: false,
            dx12: Dx12Filter::Any,
            hide_unknown_api: false,
            vulkan: false,
            min_reports: 0,
            max_reports: None,
            min_comments: 0,
            max_comments: None,
            deck: Vec::new(),
            sort: SortKey::Reports,
            order: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagState {
    Off,
    Include,
    Exclude,
}

impl Filters {
    fn to_query(&self) -> Filter {
        let name = self.name.trim();
        Filter {
            genres: self.include.clone(),
            any_genre: self.any_genre,
            top_tags: (self.top_tags < ALL_TAGS).then_some(self.top_tags),
            exclude_genres: self.exclude.clone(),
            tiers: self.tiers.iter().copied().collect(),
            min_tier: None,
            tier_source: self.tier_source,
            exclude_native: self.proton_only,
            dx12: self.dx12,
            hide_unknown_api: self.hide_unknown_api,
            vulkan: self.vulkan,
            min_reports: (self.min_reports > 0).then_some(self.min_reports),
            max_reports: self.max_reports,
            min_comments: (self.min_comments > 0).then_some(self.min_comments),
            max_comments: self.max_comments,
            deck: self.deck.clone(),
            name: (!name.is_empty()).then(|| name.to_string()),
            sort: self.sort,
            order: self.order,
            limit: None,
        }
    }

    fn tag_state(&self, tag: &str) -> TagState {
        if self.include.iter().any(|t| t == tag) {
            TagState::Include
        } else if self.exclude.iter().any(|t| t == tag) {
            TagState::Exclude
        } else {
            TagState::Off
        }
    }

    fn set_tag(&mut self, tag: &str, state: TagState) {
        self.include.retain(|t| t != tag);
        self.exclude.retain(|t| t != tag);
        match state {
            TagState::Include => self.include.push(tag.to_string()),
            TagState::Exclude => self.exclude.push(tag.to_string()),
            TagState::Off => {}
        }
    }

    /// Clears every filter but keeps the sort order.
    fn reset(&mut self) {
        *self = Filters {
            sort: self.sort,
            order: self.order,
            ..Default::default()
        };
    }

    fn is_default(&self) -> bool {
        let mut plain = self.clone();
        plain.reset();
        *self == plain
    }
}

// ---------------------------------------------------------------------------
// Query worker

enum Request {
    Search(u64, Filter),
    Overview,
}

enum Response {
    Search(u64, Result<(Vec<Game>, usize), String>),
    Overview(Result<Overview, String>),
}

#[derive(Debug, Clone, Default)]
struct Overview {
    status: Status,
    /// Every tag in use, most common first.
    tags: Vec<(String, i64)>,
    max_reports: u32,
    max_comments: u32,
}

fn overview(db: &Db) -> Result<Overview> {
    let (max_reports, max_comments) = db.max_counts()?;
    Ok(Overview {
        status: db.status()?,
        tags: db.tag_counts()?,
        max_reports,
        max_comments,
    })
}

fn spawn_query_worker(
    db_path: PathBuf,
    ctx: egui::Context,
) -> (mpsc::Sender<Request>, mpsc::Receiver<Response>) {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (res_tx, res_rx) = mpsc::channel();
    thread::spawn(move || {
        let db = Db::open(&db_path).map_err(|e| format!("{e:#}"));
        while let Ok(first) = req_rx.recv() {
            // Typing queues a search per keystroke; only the newest one matters.
            let (mut search, mut want_overview) = (None, false);
            for req in std::iter::once(first).chain(req_rx.try_iter()) {
                match req {
                    Request::Search(generation, filter) => search = Some((generation, filter)),
                    Request::Overview => want_overview = true,
                }
            }
            let db = match &db {
                Ok(db) => db,
                Err(e) => {
                    if let Some((generation, _)) = search {
                        let _ = res_tx.send(Response::Search(generation, Err(e.clone())));
                    }
                    ctx.request_repaint();
                    continue;
                }
            };
            if want_overview {
                let _ = res_tx.send(Response::Overview(
                    overview(db).map_err(|e| format!("{e:#}")),
                ));
            }
            if let Some((generation, filter)) = search {
                let result = db.search(&filter).map_err(|e| format!("{e:#}"));
                if res_tx.send(Response::Search(generation, result)).is_err() {
                    break;
                }
            }
            ctx.request_repaint();
        }
    });
    (req_tx, res_rx)
}

/// Runs async network work on its own thread and runtime, then wakes the UI.
fn spawn_network_job<F, Fut>(ctx: &egui::Context, job: F) -> mpsc::Receiver<Result<(), String>>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    thread::spawn(move || {
        let result = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(job()));
        let _ = tx.send(result.map_err(|e| format!("{e:#}")));
        ctx.request_repaint();
    });
    rx
}

struct SyncJob {
    progress: Arc<SyncProgress>,
    done: mpsc::Receiver<Result<(), String>>,
}

struct RefreshJob {
    appid: u32,
    done: mpsc::Receiver<Result<(), String>>,
}

// ---------------------------------------------------------------------------
// App

struct App {
    db_path: PathBuf,
    cache_dir: PathBuf,
    filters: Filters,
    /// Filters behind the results on screen (or in flight).
    sent: Option<Filters>,
    generation: u64,
    requests: mpsc::Sender<Request>,
    responses: mpsc::Receiver<Response>,
    results: Vec<Game>,
    total: usize,
    searching: bool,
    error: Option<String>,
    overview: Option<Overview>,
    selected: Option<u32>,
    tag_query: String,
    sync: Option<SyncJob>,
    sync_log: Vec<String>,
    sync_error: Option<String>,
    show_sync_log: bool,
    refresh: Option<RefreshJob>,
    refresh_note: Option<(u32, String)>,
    /// PCGamingWiki lookup for a selected game the sync hasn't reached.
    wiki: Option<RefreshJob>,
    wiki_note: Option<(u32, String)>,
    /// Games already looked up this session, so a failure isn't retried every frame.
    wiki_tried: HashSet<u32>,
    last_live_update: f64,
    capture: Option<capture::Capture>,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        db_path: PathBuf,
        cache_dir: PathBuf,
        mut capture: Option<capture::Capture>,
    ) -> Self {
        let mut filters = cc
            .storage
            .and_then(|s| eframe::get_value::<Filters>(s, STORAGE_KEY))
            .unwrap_or_default();
        let (requests, responses) = spawn_query_worker(db_path.clone(), cc.egui_ctx.clone());
        let _ = requests.send(Request::Overview);
        let (mut selected, mut tag_query) = (None, String::new());
        if let Some(c) = &mut capture {
            filters = c.state.filters.take().unwrap_or_default();
            selected = c.state.select;
            tag_query = std::mem::take(&mut c.state.tag_query);
            if let Some(dark) = c.state.dark {
                cc.egui_ctx.set_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                });
            }
        }
        App {
            db_path,
            cache_dir,
            filters,
            sent: None,
            generation: 0,
            requests,
            responses,
            results: Vec::new(),
            total: 0,
            searching: false,
            error: None,
            overview: None,
            selected,
            tag_query,
            sync: None,
            sync_log: Vec::new(),
            sync_error: None,
            show_sync_log: false,
            refresh: None,
            refresh_note: None,
            wiki: None,
            wiki_note: None,
            wiki_tried: HashSet::new(),
            last_live_update: 0.0,
            capture,
        }
    }

    /// Re-reads everything from the database, e.g. after a sync.
    fn reload(&mut self) {
        let _ = self.requests.send(Request::Overview);
        self.sent = None;
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(response) = self.responses.try_recv() {
            match response {
                Response::Search(generation, result) if generation == self.generation => {
                    self.searching = false;
                    match result {
                        Ok((games, total)) => {
                            self.results = games;
                            self.total = total;
                            self.error = None;
                        }
                        Err(e) => {
                            self.results.clear();
                            self.total = 0;
                            self.error = Some(e);
                        }
                    }
                }
                Response::Search(..) => {}
                Response::Overview(Ok(o)) => self.overview = Some(o),
                Response::Overview(Err(e)) => self.error = Some(e),
            }
        }

        if let Some(job) = &self.sync {
            self.sync_log = job.progress.messages();
            match job.done.try_recv() {
                Ok(result) => {
                    self.sync_error = result.err();
                    self.sync = None;
                    self.reload();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.sync_error = Some("sync stopped unexpectedly".into());
                    self.sync = None;
                    self.reload();
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Show ratings as they land without hammering the database.
                    let now = ctx.input(|i| i.time);
                    if now - self.last_live_update > 5.0 {
                        self.last_live_update = now;
                        self.reload();
                    }
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
            }
        }

        if let Some(job) = &self.refresh
            && let Ok(result) = job.done.try_recv()
        {
            let note = match result {
                Ok(()) => "Updated just now".to_string(),
                Err(e) => format!("Couldn't refresh: {e}"),
            };
            self.refresh_note = Some((job.appid, note));
            self.refresh = None;
            self.reload();
        }

        if let Some(job) = &self.wiki
            && let Ok(result) = job.done.try_recv()
        {
            self.wiki_note = result
                .err()
                .map(|e| (job.appid, format!("Couldn't reach PCGamingWiki: {e}")));
            self.wiki = None;
            self.reload();
        }
    }

    fn send_query_if_changed(&mut self) {
        if self.sent.as_ref() == Some(&self.filters) {
            return;
        }
        self.generation += 1;
        self.searching = true;
        let _ = self
            .requests
            .send(Request::Search(self.generation, self.filters.to_query()));
        self.sent = Some(self.filters.clone());
    }

    fn start_sync(&mut self, ctx: &egui::Context) {
        let progress = Arc::new(SyncProgress::default());
        let (db_path, cache_dir, p) = (
            self.db_path.clone(),
            self.cache_dir.clone(),
            progress.clone(),
        );
        let done = spawn_network_job(ctx, move || async move {
            let mut db = Db::open(&db_path)?;
            let client = http::client()?;
            let mut opts = SyncOptions::new(cache_dir);
            opts.progress = Some(p);
            sync::run(&mut db, &client, &opts).await
        });
        self.sync_error = None;
        self.sync_log.clear();
        self.sync = Some(SyncJob { progress, done });
    }

    fn start_refresh(&mut self, ctx: &egui::Context, appid: u32) {
        let db_path = self.db_path.clone();
        let done = spawn_network_job(ctx, move || async move {
            let mut db = Db::open(&db_path)?;
            let client = http::client()?;
            sync::refresh_game(&mut db, &client, appid).await
        });
        self.refresh_note = None;
        self.refresh = Some(RefreshJob { appid, done });
    }

    fn start_wiki_lookup(&mut self, ctx: &egui::Context, appid: u32) {
        let db_path = self.db_path.clone();
        let done = spawn_network_job(ctx, move || async move {
            let mut db = Db::open(&db_path)?;
            sync::lookup_wiki_game(&mut db, appid).await
        });
        self.wiki_note = None;
        self.wiki = Some(RefreshJob { appid, done });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("filters")
            .resizable(true)
            .default_size(300.0)
            .size_range(250.0..=460.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.filter_panel(ui));
            });
        let selected = self
            .selected
            .and_then(|id| self.results.iter().find(|g| g.appid == id).cloned());
        // Fill in graphics APIs for a game the sync hasn't looked up.
        if let Some(g) = &selected
            && matches!(g.wiki_lookup, WikiLookup::Pending | WikiLookup::NameMissed)
            && self.wiki.is_none()
            && self.refresh.is_none()
            && self.wiki_tried.insert(g.appid)
        {
            self.start_wiki_lookup(&ctx, g.appid);
        }
        if let Some(game) = selected {
            egui::Panel::right("details")
                .resizable(true)
                .default_size(320.0)
                .size_range(260.0..=520.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.details(ui, &game));
                });
        }
        egui::CentralPanel::default().show(ui, |ui| self.results_table(ui));

        self.send_query_if_changed();

        let (want_sync, want_refresh) = match &mut self.capture {
            Some(c) if !c.jobs_started => {
                c.jobs_started = true;
                (c.state.sync, c.state.refresh)
            }
            _ => (false, false),
        };
        if want_sync {
            self.start_sync(&ctx);
            self.show_sync_log = true;
        }
        if want_refresh && let Some(appid) = self.selected {
            self.start_refresh(&ctx, appid);
        }
        let ready = !self.searching
            && self.overview.is_some()
            && self.sent.as_ref() == Some(&self.filters)
            && self.sync.is_none()
            && self.refresh.is_none()
            && self.wiki.is_none();
        if let Some(c) = &mut self.capture {
            c.frame(&ctx, ready);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.capture.is_none() {
            eframe::set_value(storage, STORAGE_KEY, &self.filters);
        }
    }
}

// ---------------------------------------------------------------------------
// Top bar

impl App {
    fn top_bar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(RichText::new("ProtonFilter").strong());
            ui.add_space(12.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.filters.name)
                    .hint_text("Search games by name…")
                    .desired_width(300.0),
            );
            if !self.filters.name.is_empty()
                && ui.small_button("✕").on_hover_text("Clear").clicked()
            {
                self.filters.name.clear();
            }
            ui.add_space(8.0);
            let count = if self.searching && self.results.is_empty() {
                "Searching…".to_string()
            } else {
                format!("{} games", thousands(self.total))
            };
            ui.label(RichText::new(count).strong());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                self.sync_controls(ui);
            });
        });
        ui.add_space(4.0);
    }

    fn sync_controls(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if let Some(job) = &self.sync {
            if ui
                .button("Stop")
                .on_hover_text("Stop after the current batch; progress is kept")
                .clicked()
            {
                job.progress.cancel();
            }
            match job.progress.current() {
                Some(p) if p.length > 0 => {
                    let fraction = p.position as f32 / p.length as f32;
                    let text = match p.unit {
                        Unit::Games => format!(
                            "{}: {} / {}",
                            p.message,
                            thousands(p.position as usize),
                            thousands(p.length as usize)
                        ),
                        Unit::Bytes => format!("{}: {:.0}%", p.message, fraction * 100.0),
                    };
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(320.0)
                            .text(text),
                    );
                }
                Some(p) => {
                    ui.spinner();
                    ui.label(p.message);
                }
                None => {
                    ui.spinner();
                    ui.label("Syncing…");
                }
            }
            if job.progress.is_cancelled() {
                ui.label(RichText::new("stopping…").weak());
            }
            return;
        }

        if ui
            .button("⟳ Sync")
            .on_hover_text(
                "Download the latest ProtonDB export, Steam data, ratings and\n\
                 graphics APIs from PCGamingWiki.\n\
                 Only out-of-date games are fetched. You can keep browsing while it runs.",
            )
            .clicked()
        {
            self.start_sync(&ctx);
        }
        if !self.sync_log.is_empty() || self.sync_error.is_some() {
            let label = if self.sync_error.is_some() {
                "Sync failed"
            } else {
                "Last sync"
            };
            ui.toggle_value(&mut self.show_sync_log, label);
        }
        if let Some(o) = &self.overview {
            let s = &o.status;
            let rated = if s.games > 0 {
                s.with_summary * 100 / s.games
            } else {
                0
            };
            let last = s
                .last_sync
                .map(format_date)
                .unwrap_or_else(|| "never".into());
            ui.label(
                RichText::new(format!(
                    "{} games · {rated}% rated · synced {last}",
                    thousands(s.games as usize)
                ))
                .weak(),
            )
            .on_hover_text(format!(
                "ProtonDB export: {}\nRated: {}\nSteam data: {}\nNative Linux: {}\n\
                 Graphics APIs known: {} (PCGamingWiki checked {})",
                s.dump_name.as_deref().unwrap_or("none"),
                thousands(s.with_summary as usize),
                thousands(s.with_store_data as usize),
                thousands(s.native as usize),
                thousands(s.with_apis as usize),
                thousands(s.wiki_checked as usize),
            ));
        }
        if ui
            .small_button("↻")
            .on_hover_text("Reload from the database")
            .clicked()
        {
            self.reload();
        }

        if self.show_sync_log {
            let mut open = true;
            egui::Window::new("Sync log")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(460.0)
                .anchor(egui::Align2::RIGHT_TOP, [-12.0, 44.0])
                .show(&ctx, |ui| {
                    for line in &self.sync_log {
                        ui.label(line);
                    }
                    if let Some(e) = &self.sync_error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                });
            self.show_sync_log = open;
        }
    }
}

// ---------------------------------------------------------------------------
// Filter panel

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).strong().size(15.0));
    ui.add_space(2.0);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak().size(12.0));
}

impl App {
    fn filter_panel(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Filters");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!self.filters.is_default(), egui::Button::new("Reset all"))
                    .clicked()
                {
                    self.filters.reset();
                }
            });
        });

        // --- Compatibility
        section(ui, "Compatibility");
        ui.horizontal_wrapped(|ui| {
            for tier in TIERS {
                let on = self.filters.tiers.contains(&tier);
                let color = tier_color(tier, ui.visuals().dark_mode);
                let mut text = RichText::new(title_case(tier.as_str())).color(color);
                if on {
                    text = text.strong();
                }
                let resp = ui
                    .add(egui::Button::selectable(on, text))
                    .on_hover_text(tier_help(tier));
                if resp.clicked() {
                    if on {
                        self.filters.tiers.remove(&tier);
                    } else {
                        self.filters.tiers.insert(tier);
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            if ui.small_button("Gold or better").clicked() {
                self.filters.tiers = [Tier::Native, Tier::Platinum, Tier::Gold].into();
            }
            if ui.small_button("Works well (silver+)").clicked() {
                self.filters.tiers =
                    [Tier::Native, Tier::Platinum, Tier::Gold, Tier::Silver].into();
            }
        });
        hint(ui, "Pick none to see every tier.");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Rating");
            egui::ComboBox::from_id_salt("tier_source")
                .selected_text(match self.filters.tier_source {
                    TierSource::Current => "Current tier",
                    TierSource::Trending => "Trending (recent reports)",
                    TierSource::Best => "Best ever reported",
                })
                .width(190.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.filters.tier_source,
                        TierSource::Current,
                        "Current tier",
                    );
                    ui.selectable_value(
                        &mut self.filters.tier_source,
                        TierSource::Trending,
                        "Trending (recent reports)",
                    );
                    ui.selectable_value(
                        &mut self.filters.tier_source,
                        TierSource::Best,
                        "Best ever reported",
                    );
                });
        });
        ui.checkbox(
            &mut self.filters.proton_only,
            "Hide games with a native Linux build",
        );

        // --- Steam Deck
        section(ui, "Steam Deck");
        ui.horizontal_wrapped(|ui| {
            for deck in DECKS {
                let on = self.filters.deck.contains(&deck);
                if ui
                    .add(egui::Button::selectable(on, title_case(deck.as_str())))
                    .clicked()
                {
                    if on {
                        self.filters.deck.retain(|d| *d != deck);
                    } else {
                        self.filters.deck.push(deck);
                    }
                }
            }
        });

        // --- Graphics API
        section(ui, "Graphics API");
        ui.radio_value(&mut self.filters.dx12, Dx12Filter::Any, "Show DX12 games");
        ui.radio_value(
            &mut self.filters.dx12,
            Dx12Filter::NotRequired,
            "Hide games that need DX12",
        )
        .on_hover_text(
            "Hides games where DirectX 12 is the only option. Games that also offer \
             DX11, Vulkan or OpenGL stay; some need a launch option such as -dx11 to switch.",
        );
        ui.radio_value(
            &mut self.filters.dx12,
            Dx12Filter::Exclude,
            "Hide every game with DX12",
        )
        .on_hover_text("Also hides games that offer DX12 alongside DX11 or Vulkan.");
        ui.add_space(4.0);
        ui.checkbox(&mut self.filters.vulkan, "Only games with Vulkan")
            .on_hover_text(
                "Games with a Vulkan renderer run without DirectX translation \
                 (DXVK or VKD3D-Proton). Some start in DirectX and need an in-game \
                 setting or a launch option such as -vulkan to switch.",
            );
        ui.add_enabled(
            !self.filters.vulkan,
            egui::Checkbox::new(
                &mut self.filters.hide_unknown_api,
                "Hide games with unknown graphics API",
            ),
        )
        .on_disabled_hover_text("Only games with Vulkan already hides them.");
        let known = self.overview.as_ref().map(|o| &o.status);
        match known {
            Some(s) if s.with_apis == 0 => hint(ui, "No graphics API data yet. Run a sync."),
            Some(s) => hint(
                ui,
                &format!(
                    "Known for {} of {} games, from PCGamingWiki.",
                    thousands(s.with_apis as usize),
                    thousands(s.games as usize)
                ),
            ),
            None => {}
        }

        // --- Genres and tags
        section(ui, "Genres & tags");
        hint(
            ui,
            "Click once to require, twice to exclude, three times to clear.",
        );
        ui.add_space(2.0);
        let counts: std::collections::HashMap<&str, i64> = self
            .overview
            .as_ref()
            .map(|o| o.tags.iter().map(|(n, c)| (n.as_str(), *c)).collect())
            .unwrap_or_default();
        let mut changes = Vec::new();
        ui.horizontal_wrapped(|ui| {
            for genre in STEAM_GENRES {
                let state = self.filters.tag_state(genre);
                if let Some(next) = tag_chip(ui, genre, state, counts.get(genre).copied()) {
                    changes.push((genre.to_string(), next));
                }
            }
        });
        let extra: Vec<String> = self
            .filters
            .include
            .iter()
            .chain(&self.filters.exclude)
            .filter(|t| !STEAM_GENRES.contains(&t.as_str()))
            .cloned()
            .collect();
        if !extra.is_empty() {
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                for tag in &extra {
                    let state = self.filters.tag_state(tag);
                    if let Some(next) = tag_chip(ui, tag, state, counts.get(tag.as_str()).copied())
                    {
                        changes.push((tag.clone(), next));
                    }
                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                        changes.push((tag.clone(), TagState::Off));
                    }
                }
            });
        }
        for (tag, state) in changes {
            self.filters.set_tag(&tag, state);
        }

        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.tag_query)
                .hint_text("Add any Steam tag: Roguelike, Co-op…")
                .desired_width(f32::INFINITY),
        );
        let needle = self.tag_query.trim().to_lowercase();
        if !needle.is_empty()
            && let Some(o) = &self.overview
        {
            let matches: Vec<&(String, i64)> = o
                .tags
                .iter()
                .filter(|(name, _)| {
                    name.to_lowercase().contains(&needle)
                        && self.filters.tag_state(name) == TagState::Off
                })
                .take(10)
                .collect();
            if matches.is_empty() {
                hint(ui, "No matching tags.");
            }
            let mut picked = None;
            for (name, count) in matches {
                let resp = ui
                    .selectable_label(false, format!("{name}   ({})", thousands(*count as usize)));
                if resp.clicked() {
                    picked = Some((name.clone(), TagState::Include));
                }
                if resp.secondary_clicked() {
                    picked = Some((name.clone(), TagState::Exclude));
                }
                resp.on_hover_text("Click to require, right-click to exclude");
            }
            if let Some((name, state)) = picked {
                self.filters.set_tag(&name, state);
                self.tag_query.clear();
            }
        }

        if self.filters.include.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Games must have");
                ui.radio_value(&mut self.filters.any_genre, false, "all");
                ui.radio_value(&mut self.filters.any_genre, true, "any");
                ui.label("of these");
            });
        }
        if !self.filters.include.is_empty() {
            ui.add_space(4.0);
            ui.label("Only look at each game's top tags")
                .on_hover_text(
                    "Steam tags are voted by players, so a shooter can carry \"Strategy\" far down its list. \
                     Lower this to only match the tags that best describe each game.",
                );
            ui.add(
                egui::Slider::new(&mut self.filters.top_tags, 1..=ALL_TAGS).custom_formatter(
                    |n, _| {
                        if n as u32 >= ALL_TAGS {
                            "all 20".into()
                        } else {
                            format!("top {n}")
                        }
                    },
                ),
            );
        }

        // --- Activity
        let (max_reports, max_comments) = self.overview.as_ref().map_or((5000, 5000), |o| {
            (o.max_reports.max(10), o.max_comments.max(10))
        });
        section(ui, "Reports");
        count_range(
            ui,
            "reports",
            &mut self.filters.min_reports,
            &mut self.filters.max_reports,
            max_reports,
        );
        section(ui, "Comments");
        hint(
            ui,
            "Reports where the player wrote notes. From the monthly export.",
        );
        count_range(
            ui,
            "comments",
            &mut self.filters.min_comments,
            &mut self.filters.max_comments,
            max_comments,
        );
        ui.add_space(12.0);
    }
}

/// A min slider and an optional max slider over a log scale.
fn count_range(ui: &mut Ui, id: &str, min: &mut u32, max: &mut Option<u32>, cap: u32) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("At least");
            ui.add(egui::Slider::new(min, 0..=cap).logarithmic(true).integer());
            ui.end_row();
            ui.label("At most");
            let mut value = max.unwrap_or(cap);
            let resp = ui.add(
                egui::Slider::new(&mut value, 0..=cap)
                    .logarithmic(true)
                    .integer()
                    .custom_formatter(move |n, _| {
                        if n as u32 >= cap {
                            "any".into()
                        } else {
                            format!("{n:.0}")
                        }
                    }),
            );
            if resp.changed() {
                *max = (value < cap).then_some(value);
            }
            ui.end_row();
        });
    if let Some(m) = *max
        && m < *min
    {
        ui.label(
            RichText::new("“At most” is below “at least”, so nothing can match.")
                .small()
                .color(ui.visuals().warn_fg_color),
        );
    }
}

/// Draws a tri-state tag button and returns the next state if it was clicked.
fn tag_chip(ui: &mut Ui, tag: &str, state: TagState, count: Option<i64>) -> Option<TagState> {
    let text = match state {
        TagState::Off => RichText::new(tag),
        TagState::Include => RichText::new(format!("+ {tag}")).strong(),
        TagState::Exclude => RichText::new(format!("− {tag}"))
            .strikethrough()
            .color(ui.visuals().error_fg_color),
    };
    let tip = match count {
        Some(n) => format!(
            "{} games\nClick: require · again: exclude · again: clear\nRight-click: exclude",
            thousands(n as usize)
        ),
        None => "Click: require · again: exclude · again: clear\nRight-click: exclude".into(),
    };
    let resp = ui
        .add(egui::Button::selectable(state != TagState::Off, text))
        .on_hover_text(tip);
    if resp.clicked() {
        Some(match state {
            TagState::Off => TagState::Include,
            TagState::Include => TagState::Exclude,
            TagState::Exclude => TagState::Off,
        })
    } else if resp.secondary_clicked() {
        Some(if state == TagState::Exclude {
            TagState::Off
        } else {
            TagState::Exclude
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Results table

impl App {
    fn results_table(&mut self, ui: &mut Ui) {
        if let Some(err) = &self.error {
            ui.add_space(20.0);
            ui.colored_label(ui.visuals().error_fg_color, err);
            return;
        }
        if let Some(o) = &self.overview
            && o.status.games == 0
        {
            self.empty_database(ui);
            return;
        }
        if self.results.is_empty() && !self.searching {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No games match these filters.").size(16.0));
                if ui.button("Reset filters").clicked() {
                    self.filters.reset();
                }
            });
            return;
        }

        let row_height = ui.text_style_height(&egui::TextStyle::Body).max(18.0) + 6.0;
        let dark = ui.visuals().dark_mode;
        let mut clicked = None;
        let mut open = None;
        let mut sort_change = None;
        let current = (self.filters.sort, effective_order(&self.filters));

        // Narrow windows scroll sideways instead of clipping columns.
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(270.0).at_least(140.0).clip(true))
                    .column(Column::initial(84.0).at_least(70.0))
                    .column(Column::initial(62.0).at_least(50.0))
                    .column(Column::initial(96.0).at_least(70.0))
                    .column(Column::initial(104.0).at_least(60.0).clip(true))
                    .column(Column::initial(76.0).at_least(60.0))
                    .column(Column::initial(90.0).at_least(60.0))
                    .column(Column::initial(60.0).at_least(50.0))
                    .column(Column::initial(96.0).at_least(80.0))
                    .column(Column::remainder().at_least(120.0).clip(true))
                    .header(row_height + 2.0, |mut header| {
                        for (label, key) in [
                            ("Name", Some(SortKey::Name)),
                            ("Tier", Some(SortKey::Tier)),
                            ("Native", None),
                            ("Deck", None),
                            ("API", None),
                            ("Reports", Some(SortKey::Reports)),
                            ("Comments", Some(SortKey::Comments)),
                            ("Score", Some(SortKey::Score)),
                            ("Last report", Some(SortKey::Recent)),
                            ("Tags", None),
                        ] {
                            header.col(|ui| {
                                let Some(key) = key else {
                                    ui.strong(label);
                                    return;
                                };
                                let arrow = match current {
                                    (k, Order::Asc) if k == key => " ▲",
                                    (k, Order::Desc) if k == key => " ▼",
                                    _ => "",
                                };
                                let resp = ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(format!("{label}{arrow}")).strong(),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_text("Click to sort");
                                if resp.clicked() {
                                    sort_change = Some(key);
                                }
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(row_height, self.results.len(), |mut row| {
                            let g = &self.results[row.index()];
                            row.set_selected(self.selected == Some(g.appid));
                            row.col(|ui| {
                                ui.label(&g.name);
                            });
                            row.col(|ui| tier_badge(ui, g.tier.as_deref(), dark));
                            row.col(|ui| match g.native {
                                Some(true) => {
                                    ui.label(
                                        RichText::new("Yes")
                                            .color(tier_color(Tier::Native, dark))
                                            .strong(),
                                    );
                                }
                                Some(false) => {
                                    ui.label(RichText::new("No").weak());
                                }
                                None => {
                                    ui.label(RichText::new("?").weak());
                                }
                            });
                            row.col(|ui| deck_label(ui, g.deck, dark));
                            row.col(|ui| api_label(ui, g.graphics_apis.as_ref(), dark));
                            row.col(|ui| {
                                ui.label(thousands(g.reports as usize));
                            });
                            row.col(|ui| {
                                ui.label(thousands(g.comments as usize));
                            });
                            row.col(|ui| {
                                ui.label(
                                    g.score
                                        .map(|s| format!("{s:.2}"))
                                        .unwrap_or_else(|| "–".into()),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    g.last_report_at
                                        .map(format_date)
                                        .unwrap_or_else(|| "–".into()),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(
                                        g.tags
                                            .iter()
                                            .take(5)
                                            .cloned()
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                    )
                                    .weak(),
                                );
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                clicked = Some(g.appid);
                            }
                            if resp.double_clicked() {
                                open = Some(g.protondb_url.clone());
                            }
                        });
                    })
            });

        if let Some(appid) = clicked {
            self.selected = if self.selected == Some(appid) {
                None
            } else {
                Some(appid)
            };
        }
        if let Some(url) = open {
            ui.ctx().open_url(OpenUrl::new_tab(url));
        }
        if let Some(key) = sort_change {
            if self.filters.sort == key {
                self.filters.order = Some(match effective_order(&self.filters) {
                    Order::Asc => Order::Desc,
                    Order::Desc => Order::Asc,
                });
            } else {
                self.filters.sort = key;
                self.filters.order = None;
            }
        }
    }

    fn empty_database(&mut self, ui: &mut Ui) {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No data yet").size(22.0).strong());
            ui.add_space(8.0);
            ui.label(
                "Sync downloads ProtonDB's monthly export, Steam genres and Linux support, \
                 a ProtonDB rating for every game, and graphics APIs from PCGamingWiki.",
            );
            ui.label(
                "The first sync takes up to two hours because ProtonDB and PCGamingWiki limit how fast \
                 they can be asked. \
                 Games with the most reports come first, so you can start browsing within minutes.",
            );
            ui.add_space(12.0);
            if self.sync.is_none() && ui.button(RichText::new("⟳ Sync now").size(16.0)).clicked() {
                let ctx = ui.ctx().clone();
                self.start_sync(&ctx);
            }
        });
    }
}

fn effective_order(f: &Filters) -> Order {
    f.order.unwrap_or(if f.sort == SortKey::Name {
        Order::Asc
    } else {
        Order::Desc
    })
}

// ---------------------------------------------------------------------------
// Details panel

impl App {
    fn details(&mut self, ui: &mut Ui, g: &Game) {
        let dark = ui.visuals().dark_mode;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                if ui.small_button("✕").on_hover_text("Close").clicked() {
                    self.selected = None;
                }
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::TOP).with_main_wrap(true),
                    |ui| {
                        ui.heading(&g.name);
                    },
                );
            });
        });
        ui.label(RichText::new(format!("Steam app {}", g.appid)).weak());
        ui.add_space(8.0);

        egui::Grid::new("details_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Tier");
                tier_badge(ui, g.tier.as_deref(), dark);
                ui.end_row();
                ui.label("Trending");
                tier_badge(ui, g.trending_tier.as_deref(), dark);
                ui.end_row();
                ui.label("Best reported");
                tier_badge(ui, g.best_tier.as_deref(), dark);
                ui.end_row();
                if let Some(score) = g.score {
                    ui.label("Score");
                    ui.label(format!(
                        "{score:.2}  ({} confidence)",
                        g.confidence.as_deref().unwrap_or("unknown")
                    ));
                    ui.end_row();
                }
                ui.label("Native Linux");
                ui.label(match g.native {
                    Some(true) => "Yes",
                    Some(false) => "No",
                    None => "Unknown (not on the Steam store)",
                });
                ui.end_row();
                ui.label("Steam Deck");
                deck_label(ui, g.deck, dark);
                ui.end_row();
                ui.label("Graphics API");
                let looking_up = self.wiki.as_ref().is_some_and(|j| j.appid == g.appid);
                ui.vertical(|ui| match &g.graphics_apis {
                    Some(apis) => {
                        let text = RichText::new(apis.label());
                        if apis.needs_dx12() {
                            ui.label(text.color(tier_color(Tier::Bronze, dark)));
                            hint(ui, "DX12 only: runs through VKD3D-Proton");
                        } else {
                            ui.label(text);
                        }
                    }
                    None if looking_up => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Looking it up…").weak());
                        });
                    }
                    None => {
                        ui.label(
                            RichText::new(match g.wiki_lookup {
                                WikiLookup::Found => "Unknown (PCGamingWiki doesn't list it)",
                                WikiLookup::Pending => "Unknown (not looked up yet)",
                                _ => "Unknown (no PCGamingWiki page)",
                            })
                            .weak(),
                        );
                    }
                });
                ui.end_row();
                ui.label("Reports");
                ui.label(thousands(g.reports as usize));
                ui.end_row();
                ui.label("Comments");
                ui.label(thousands(g.comments as usize))
                    .on_hover_text("Reports with written notes, as of the latest monthly export");
                ui.end_row();
                if let Some(ts) = g.last_report_at {
                    ui.label("Last report");
                    ui.label(format_date(ts));
                    ui.end_row();
                }
            });

        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open on ProtonDB").clicked() {
                ui.ctx().open_url(OpenUrl::new_tab(&g.protondb_url));
            }
            if ui.button("Open on Steam").clicked() {
                ui.ctx().open_url(OpenUrl::new_tab(format!(
                    "https://store.steampowered.com/app/{}",
                    g.appid
                )));
            }
            if let Some(url) = &g.pcgamingwiki_url
                && ui.button("Open on PCGamingWiki").clicked()
            {
                ui.ctx().open_url(OpenUrl::new_tab(url));
            }
            let busy = self.refresh.is_some();
            let resp = ui
                .add_enabled(!busy, egui::Button::new("⟳ Refresh"))
                .on_hover_text("Fetch this game's latest rating, Steam data and graphics APIs now");
            if resp.clicked() {
                let ctx = ui.ctx().clone();
                self.start_refresh(&ctx, g.appid);
            }
            if busy {
                ui.spinner();
            }
        });
        for (appid, note) in [&self.refresh_note, &self.wiki_note].into_iter().flatten() {
            if *appid == g.appid {
                ui.label(RichText::new(note).small().weak());
            }
        }

        if !g.tags.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new("Tags").strong());
            hint(ui, "Click a tag to filter by it.");
            ui.add_space(2.0);
            let mut picked = None;
            ui.horizontal_wrapped(|ui| {
                for tag in &g.tags {
                    let state = self.filters.tag_state(tag);
                    if let Some(next) = tag_chip(ui, tag, state, None) {
                        picked = Some((tag.clone(), next));
                    }
                }
            });
            if let Some((tag, state)) = picked {
                self.filters.set_tag(&tag, state);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers

fn tier_color(tier: Tier, dark: bool) -> Color32 {
    let (d, l) = match tier {
        Tier::Native => ((92, 200, 120), (22, 128, 60)),
        Tier::Platinum => ((180, 199, 220), (70, 100, 140)),
        Tier::Gold => ((228, 192, 70), (150, 110, 0)),
        Tier::Silver => ((176, 176, 176), (100, 100, 100)),
        Tier::Bronze => ((205, 127, 50), (150, 85, 20)),
        Tier::Borked => ((235, 90, 90), (190, 30, 30)),
        Tier::Pending => ((130, 130, 130), (130, 130, 130)),
    };
    let (r, g, b) = if dark { d } else { l };
    Color32::from_rgb(r, g, b)
}

fn tier_badge(ui: &mut Ui, tier: Option<&str>, dark: bool) {
    match tier.and_then(Tier::parse) {
        Some(t) => {
            ui.label(
                RichText::new(format!(" {} ", t.as_str().to_uppercase()))
                    .size(12.0)
                    .strong()
                    .color(if dark {
                        Color32::from_gray(20)
                    } else {
                        Color32::WHITE
                    })
                    .background_color(tier_color(t, dark)),
            );
        }
        None => {
            ui.label(RichText::new("not rated").weak().size(12.0));
        }
    }
}

fn deck_label(ui: &mut Ui, deck: Option<Deck>, dark: bool) {
    let (text, color) = match deck {
        Some(Deck::Verified) => ("Verified", Some(tier_color(Tier::Native, dark))),
        Some(Deck::Playable) => ("Playable", Some(tier_color(Tier::Gold, dark))),
        Some(Deck::Unsupported) => ("Unsupported", Some(tier_color(Tier::Borked, dark))),
        _ => ("–", None),
    };
    match color {
        Some(c) => ui.label(RichText::new(text).color(c)),
        None => ui.label(RichText::new(text).weak()),
    };
}

/// Table cell for graphics APIs; DX12-only games stand out.
fn api_label(ui: &mut Ui, apis: Option<&GraphicsApis>, dark: bool) {
    match apis {
        Some(a) if a.needs_dx12() => {
            ui.label(RichText::new(a.short_label()).color(tier_color(Tier::Bronze, dark)))
                .on_hover_text("DirectX 12 only: runs through VKD3D-Proton");
        }
        Some(a) => {
            ui.label(a.short_label()).on_hover_text(a.label());
        }
        None => {
            ui.label(RichText::new("?").weak());
        }
    }
}

fn tier_help(tier: Tier) -> &'static str {
    match tier {
        Tier::Native => "Has a native Linux build on Steam",
        Tier::Platinum => "Runs perfectly out of the box",
        Tier::Gold => "Runs perfectly after tweaks",
        Tier::Silver => "Runs with minor issues, but generally playable",
        Tier::Bronze => "Runs, but often crashes or has issues",
        Tier::Borked => "Doesn't start or isn't playable",
        Tier::Pending => "Not enough reports yet for a rating",
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(34769), "34,769");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn tag_states_cycle_and_reset_keeps_sort() {
        let mut f = Filters::default();
        f.set_tag("RPG", TagState::Include);
        f.set_tag("Indie", TagState::Exclude);
        assert_eq!(f.tag_state("RPG"), TagState::Include);
        f.set_tag("RPG", TagState::Exclude);
        assert_eq!((f.include.len(), f.exclude.len()), (0, 2));

        f.sort = SortKey::Comments;
        f.min_reports = 50;
        assert!(!f.is_default());
        f.reset();
        assert!(f.is_default());
        assert_eq!(f.sort, SortKey::Comments);
    }

    #[test]
    fn gui_filters_map_to_queries() {
        let f = Filters {
            name: "  elden ".into(),
            top_tags: ALL_TAGS,
            min_reports: 0,
            max_comments: Some(10),
            tiers: [Tier::Gold, Tier::Native].into(),
            dx12: Dx12Filter::NotRequired,
            vulkan: true,
            ..Default::default()
        };
        let q = f.to_query();
        assert_eq!(q.dx12, Dx12Filter::NotRequired);
        assert!(!q.hide_unknown_api);
        assert!(q.vulkan);
        assert_eq!(q.name.as_deref(), Some("elden"));
        assert_eq!(q.top_tags, None);
        assert_eq!(q.min_reports, None);
        assert_eq!(q.max_comments, Some(10));
        assert_eq!(q.tiers, vec![Tier::Gold, Tier::Native]);
        assert_eq!(q.limit, None);
    }
}
