#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    str::FromStr,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::WebviewBuilder,
    window::WindowBuilder,
    AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize, RunEvent, Url, Webview,
    WebviewUrl, WebviewWindowBuilder, Window, WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_window_state::{AppHandleExt, StateFlags, WindowExt};

mod stats;
#[cfg(windows)]
mod thumbbar;

const INJECT_JS: &str = include_str!("inject.js");
const HOME_URL: &str = "https://music.youtube.com/";
// Every webview sharing a WebView2 data folder must use identical browser args,
// so all webviews use this. The first flag set is Tauri's default.
const BASE_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
                                 --autoplay-policy=no-user-gesture-required";

fn browser_args() -> String {
    // Debug builds expose the DevTools protocol on localhost (default port 9229,
    // override with YTM_DEBUG_PORT) for testing the injected script. YTM_EXTRA_BROWSER_ARGS
    // adds more flags, e.g. --mute-audio for silent test runs.
    #[cfg(debug_assertions)]
    {
        let port = std::env::var("YTM_DEBUG_PORT").unwrap_or_else(|_| "9229".into());
        let extra = std::env::var("YTM_EXTRA_BROWSER_ARGS").unwrap_or_default();
        format!("{BASE_BROWSER_ARGS} --remote-debugging-port={port} {extra}")
    }
    #[cfg(not(debug_assertions))]
    BASE_BROWSER_ARGS.to_string()
}
const PERSIST_INTERVAL: Duration = Duration::from_secs(15);
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
const MIN_RELOAD_GAP: Duration = Duration::from_secs(30 * 60);
const MEMORY_CACHE: Duration = Duration::from_secs(2);
/// How much RAM history the panel's graph can show.
const MEMORY_HISTORY: Duration = Duration::from_secs(10 * 60);
/// Stats panel width in logical pixels. When closed it's hidden entirely.
const PANEL_OPEN_W: f64 = 320.0;
const PANEL_SLIDE: Duration = Duration::from_millis(200);
/// YouTube Music's dark background, for anything visible behind or before the pages.
const WINDOW_BG: tauri::window::Color = tauri::window::Color(3, 3, 3, 255);

// ---------- persisted data ----------

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    toggle: String,
    next: String,
    prev: String,
    /// Toggles the stats panel. Only active while the app is focused, not global.
    panel: String,
    watchdog: bool,
    memory_limit_mb: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            toggle: "Ctrl+Shift+Space".into(),
            next: "Ctrl+Shift+PageUp".into(),
            prev: "Ctrl+Shift+PageDown".into(),
            panel: "Ctrl+D".into(),
            watchdog: true,
            memory_limit_mb: 1500,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct PlayerState {
    video_id: String,
    playlist_id: Option<String>,
    time: f64,
    playing: bool,
    title: String,
    artist: String,
    album: String,
    duration: f64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct UiState {
    panel_open: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self { panel_open: true }
    }
}

/// Equalizer, applied to YouTube Music's audio by inject.js. Gains are dB per band
/// (32 Hz … 16 kHz); the page adds its own headroom so boosts don't clip.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct EqState {
    enabled: bool,
    preset: String,
    gains: Vec<f32>,
}

const EQ_BANDS: usize = 10;
const EQ_MAX_DB: f32 = 12.0;

impl Default for EqState {
    fn default() -> Self {
        Self { enabled: false, preset: "flat".into(), gains: vec![0.0; EQ_BANDS] }
    }
}

/// What the next page load should restore (consumed once by the injected script).
#[derive(Clone, Serialize)]
struct Restore {
    video_id: String,
    time: f64,
    autoplay: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Action {
    Toggle,
    Next,
    Prev,
}

impl Action {
    fn js(self) -> &'static str {
        match self {
            Action::Toggle => "toggle",
            Action::Next => "next",
            Action::Prev => "prev",
        }
    }
}

struct AppState {
    settings: Mutex<Settings>,
    shortcuts: Mutex<Vec<(Shortcut, Action)>>,
    player: Mutex<PlayerState>,
    last_persist: Mutex<Instant>,
    restore: Mutex<Option<Restore>>,
    reload_pending: Mutex<bool>,
    last_reload: Mutex<Option<Instant>>,
    stats: Mutex<Option<stats::Stats>>,
    ui: Mutex<UiState>,
    eq: Mutex<EqState>,
    started: Instant,
    memory: Mutex<MemoryCache>,
    /// Bumped on every panel toggle so an older slide animation stops.
    slide_gen: std::sync::atomic::AtomicU64,
}

struct MemoryCache {
    sys: sysinfo::System,
    at: Option<Instant>,
    mb: u64,
    /// (unix ms, MB) samples for the panel's graph.
    history: std::collections::VecDeque<(u64, u64)>,
}

fn data_dir(app: &AppHandle) -> PathBuf {
    let dir = app.path().app_data_dir().expect("no app data dir");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: PathBuf) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(path: PathBuf, value: &T) {
    // Write to a temp file then rename, so a power cut never leaves a half-written file.
    let tmp = path.with_extension("tmp");
    if let Ok(json) = serde_json::to_string_pretty(value) {
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

fn persist_player(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let player = state.player.lock().unwrap().clone();
    if !player.video_id.is_empty() {
        save_json(data_dir(app).join("player.json"), &player);
    }
    *state.last_persist.lock().unwrap() = Instant::now();
}

fn watch_url(p: &PlayerState) -> Url {
    let mut url = Url::parse("https://music.youtube.com/watch").unwrap();
    url.query_pairs_mut().append_pair("v", &p.video_id);
    if let Some(list) = p.playlist_id.as_deref().filter(|l| !l.is_empty()) {
        url.query_pairs_mut().append_pair("list", list);
    }
    url
}

fn song_title(p: &PlayerState) -> String {
    match (p.title.is_empty(), p.artist.is_empty()) {
        (true, _) => "YouTube Music".into(),
        (false, true) => p.title.clone(),
        (false, false) => format!("{} – {}", p.title, p.artist),
    }
}

/// Shown in the taskbar hover preview and the tray tooltip.
fn show_song(app: &AppHandle, p: &PlayerState) {
    let title = song_title(p);
    if let Some(w) = main_window(app) {
        let _ = w.set_title(&title);
    }
    if let Some(tray) = app.tray_by_id("tray") {
        let _ = tray.set_tooltip(Some(&title));
    }
}

/// Window geometry to remember. Visibility is left out so the app always opens shown.
fn window_state_flags() -> StateFlags {
    StateFlags::all() - StateFlags::VISIBLE
}

fn save_window_state(app: &AppHandle) {
    let _ = app.save_window_state(window_state_flags());
}

// ---------- player control ----------

/// The window holding both webviews: "ytm" (YouTube Music) and "panel" (stats).
fn main_window(app: &AppHandle) -> Option<Window> {
    app.get_window("main")
}

fn ytm_webview(app: &AppHandle) -> Option<Webview> {
    app.get_webview("ytm")
}

pub(crate) fn run_action(app: &AppHandle, action: Action) {
    if let Some(w) = ytm_webview(app) {
        let _ = w.eval(format!("window.__ytmd && window.__ytmd.{}()", action.js()));
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = main_window(app) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(w) = ytm_webview(app) {
        let _ = w.set_focus();
    }
}

/// Place YouTube Music on the left and the stats panel on the right.
fn layout(app: &AppHandle) {
    let Some(win) = main_window(app) else { return };
    let Ok(size) = win.inner_size() else { return };
    if size.width == 0 || size.height == 0 {
        return; // minimized
    }
    let scale = win.scale_factor().unwrap_or(1.0);
    let open = app.state::<AppState>().ui.lock().unwrap().panel_open;
    let panel_w = if open { ((PANEL_OPEN_W * scale).round() as u32).min(size.width) } else { 0 };
    let ytm_w = size.width - panel_w;
    if let Some(w) = ytm_webview(app) {
        let _ = w.set_position(PhysicalPosition::new(0, 0));
        let _ = w.set_size(PhysicalSize::new(ytm_w, size.height));
    }
    if let Some(w) = app.get_webview("panel") {
        if open {
            let _ = w.set_position(PhysicalPosition::new(ytm_w as i32, 0));
            let _ = w.set_size(PhysicalSize::new(panel_w, size.height));
            let _ = w.show();
        } else {
            let _ = w.hide();
        }
    }
}

fn reload_player(app: &AppHandle, autoplay: bool) {
    let state = app.state::<AppState>();
    let player = state.player.lock().unwrap().clone();
    let Some(w) = ytm_webview(app) else { return };
    *state.reload_pending.lock().unwrap() = false;
    *state.last_reload.lock().unwrap() = Some(Instant::now());
    if player.video_id.is_empty() {
        let _ = w.navigate(Url::parse(HOME_URL).unwrap());
        return;
    }
    *state.restore.lock().unwrap() = Some(Restore {
        video_id: player.video_id.clone(),
        time: if autoplay { 0.0 } else { player.time },
        autoplay,
    });
    let _ = w.navigate(watch_url(&player));
}

// ---------- shortcuts ----------

fn apply_shortcuts(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let mut parsed = Vec::new();
    for (accel, action, name) in [
        (&s.toggle, Action::Toggle, "Play/Pause"),
        (&s.next, Action::Next, "Next"),
        (&s.prev, Action::Prev, "Previous"),
    ] {
        if accel.trim().is_empty() {
            continue;
        }
        let sc = Shortcut::from_str(accel).map_err(|e| format!("{name}: invalid shortcut \"{accel}\" ({e})"))?;
        parsed.push((sc, action, name));
    }
    let mut seen = HashSet::new();
    for (sc, _, name) in &parsed {
        if !seen.insert(sc.id()) {
            return Err(format!("{name}: the same shortcut is used twice"));
        }
    }

    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    for (sc, _, name) in &parsed {
        gs.register(*sc)
            .map_err(|e| format!("{name}: could not register (another app may be using it): {e}"))?;
    }
    *app.state::<AppState>().shortcuts.lock().unwrap() =
        parsed.into_iter().map(|(sc, a, _)| (sc, a)).collect();
    Ok(())
}

// ---------- memory watchdog ----------

/// App memory, recomputed at most every few seconds (the panel polls it).
fn cached_memory_mb(app: &AppHandle) -> u64 {
    let state = app.state::<AppState>();
    let mut m = state.memory.lock().unwrap();
    if m.at.is_none_or(|t| t.elapsed() >= MEMORY_CACHE) {
        m.mb = app_memory_mb(&mut m.sys);
        m.at = Some(Instant::now());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mb = m.mb;
        m.history.push_back((now_ms, mb));
        while m.history.front().is_some_and(|(t, _)| now_ms - t > MEMORY_HISTORY.as_millis() as u64) {
            m.history.pop_front();
        }
    }
    m.mb
}

/// Total memory (MB) of every process descended from this app (the WebView2 tree).
fn app_memory_mb(sys: &mut sysinfo::System) -> u64 {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_memory());
    let Ok(me) = sysinfo::get_current_pid() else { return 0 };
    let procs = sys.processes();
    let mut total = 0u64;
    for (pid, proc_) in procs {
        let mut cur = Some(*pid);
        let mut hops = 0;
        while let Some(p) = cur {
            if p == me {
                total += proc_.memory();
                break;
            }
            hops += 1;
            if hops > 8 {
                break;
            }
            cur = procs.get(&p).and_then(|x| x.parent());
        }
    }
    total / 1024 / 1024
}

fn start_watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(WATCHDOG_INTERVAL);
            let state = app.state::<AppState>();
            let (enabled, limit) = {
                let s = state.settings.lock().unwrap();
                (s.watchdog, s.memory_limit_mb)
            };
            if !enabled {
                continue;
            }
            if let Some(t) = *state.last_reload.lock().unwrap() {
                if t.elapsed() < MIN_RELOAD_GAP {
                    continue;
                }
            }
            let mb = cached_memory_mb(&app);
            if mb < limit {
                continue;
            }
            let playing = state.player.lock().unwrap().playing;
            let focused = main_window(&app).and_then(|w| w.is_focused().ok()).unwrap_or(false);
            if !playing && !focused {
                eprintln!("[watchdog] {mb} MB >= {limit} MB, reloading now (paused)");
                reload_player(&app, false);
            } else if playing {
                // Reload at the next track change so the music isn't cut mid-song.
                eprintln!("[watchdog] {mb} MB >= {limit} MB, reload queued for next track");
                *state.reload_pending.lock().unwrap() = true;
            }
        }
    });
}

// ---------- commands ----------

#[tauri::command]
fn report_state(app: AppHandle, webview: Webview, state: PlayerState) {
    if webview.label() != "ytm" || state.video_id.is_empty() {
        return;
    }
    let s = app.state::<AppState>();
    if let Some(db) = s.stats.lock().unwrap().as_mut() {
        if let Err(e) = db.record(&state) {
            eprintln!("stats: {e}");
        }
    }
    let (track_changed, play_changed, title_changed) = {
        let mut cur = s.player.lock().unwrap();
        let changes = (
            cur.video_id != state.video_id,
            cur.playing != state.playing,
            song_title(&cur) != song_title(&state),
        );
        *cur = state.clone();
        changes
    };

    if title_changed {
        show_song(&app, &state);
    }
    #[cfg(windows)]
    if play_changed {
        let playing = state.playing;
        let _ = app.run_on_main_thread(move || thumbbar::set_playing(playing));
    }

    if track_changed && state.playing && *s.reload_pending.lock().unwrap() {
        persist_player(&app);
        reload_player(&app, true);
        return;
    }

    let due = s.last_persist.lock().unwrap().elapsed() >= PERSIST_INTERVAL;
    if track_changed || play_changed || due {
        persist_player(&app);
    }
}

#[tauri::command]
fn get_restore(app: AppHandle, webview: Webview) -> Option<Restore> {
    if webview.label() != "ytm" {
        return None;
    }
    app.state::<AppState>().restore.lock().unwrap().take()
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Settings {
    app.state::<AppState>().settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, webview: Webview, settings: Settings) -> Result<(), String> {
    if webview.label() != "settings" {
        return Err("not allowed".into());
    }
    if let Err(e) = apply_shortcuts(&app, &settings) {
        // Put the previous, working shortcuts back.
        let old = app.state::<AppState>().settings.lock().unwrap().clone();
        let _ = apply_shortcuts(&app, &old);
        return Err(e);
    }
    save_json(data_dir(&app).join("settings.json"), &settings);
    let key = serde_json::to_string(&settings.panel).unwrap_or_default();
    for label in ["ytm", "panel"] {
        if let Some(w) = app.get_webview(label) {
            let _ = w.eval(format!("window.__ytmdSetPanelKey && window.__ytmdSetPanelKey({key})"));
        }
    }
    *app.state::<AppState>().settings.lock().unwrap() = settings;
    Ok(())
}

#[tauri::command]
fn suspend_shortcuts(app: AppHandle, webview: Webview, suspend: bool) {
    // While recording a new combo in Settings, the old global shortcut would swallow it.
    if webview.label() != "settings" {
        return;
    }
    if suspend {
        let _ = app.global_shortcut().unregister_all();
    } else {
        let s = app.state::<AppState>().settings.lock().unwrap().clone();
        let _ = apply_shortcuts(&app, &s);
    }
}

#[tauri::command]
async fn get_memory_mb(app: AppHandle) -> u64 {
    cached_memory_mb(&app)
}

#[derive(Serialize)]
struct PanelUi {
    open: bool,
    key: String,
}

#[tauri::command]
fn get_panel_ui(app: AppHandle) -> PanelUi {
    let s = app.state::<AppState>();
    let open = s.ui.lock().unwrap().panel_open;
    let key = s.settings.lock().unwrap().panel.clone();
    PanelUi { open, key }
}

#[tauri::command]
fn toggle_panel(app: AppHandle) {
    let ui = {
        let s = app.state::<AppState>();
        let mut ui = s.ui.lock().unwrap();
        ui.panel_open = !ui.panel_open;
        ui.clone()
    };
    save_json(data_dir(&app).join("ui.json"), &ui);
    if let Some(w) = app.get_webview("panel") {
        let _ = w.eval(format!("window.__panelSetOpen && window.__panelSetOpen({})", ui.panel_open));
    }
    slide_panel(app, ui.panel_open);
}

/// Slide the panel in/out over the edge of YouTube Music. YouTube Music itself is
/// resized only once (before closing, after opening) so the animation stays smooth.
fn slide_panel(app: AppHandle, open: bool) {
    use std::sync::atomic::Ordering;
    let gen = app.state::<AppState>().slide_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let set_reopen_button = |app: &AppHandle, open: bool| {
        if let Some(w) = ytm_webview(app) {
            let _ = w.eval(format!("window.__ytmdSetPanelOpen && window.__ytmdSetPanelOpen({open})"));
        }
    };
    let (Some(win), Some(panel)) = (main_window(&app), app.get_webview("panel")) else { return };
    let (Ok(size), Ok(scale)) = (win.inner_size(), win.scale_factor()) else { return };
    if size.width == 0 {
        layout(&app);
        return;
    }
    let panel_w = ((PANEL_OPEN_W * scale).round() as u32).min(size.width);

    if open {
        set_reopen_button(&app, true);
        let _ = panel.set_size(PhysicalSize::new(panel_w, size.height));
        let _ = panel.set_position(PhysicalPosition::new(size.width as i32, 0));
        let _ = panel.show();
    } else if let Some(w) = ytm_webview(&app) {
        // Widen YouTube Music underneath first; the panel still covers the edge.
        let _ = w.set_size(PhysicalSize::new(size.width, size.height));
    }

    std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            if app.state::<AppState>().slide_gen.load(Ordering::SeqCst) != gen {
                return; // toggled again mid-slide
            }
            let t = (start.elapsed().as_secs_f64() / PANEL_SLIDE.as_secs_f64()).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3); // ease-out cubic
            let shown = if open { eased } else { 1.0 - eased };
            let x = size.width as f64 - panel_w as f64 * shown;
            let _ = panel.set_position(PhysicalPosition::new(x.round() as i32, 0));
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        layout(&app);
        if !open {
            set_reopen_button(&app, false);
        }
    });
}

// ---------- ban list ----------

fn with_db<T>(app: &AppHandle, f: impl FnOnce(&stats::Stats) -> rusqlite::Result<T>) -> Result<T, String> {
    match app.state::<AppState>().stats.lock().unwrap().as_ref() {
        Some(db) => f(db).map_err(|e| e.to_string()),
        None => Err("stats database unavailable".into()),
    }
}

/// Send the current ban list to both pages.
fn broadcast_bans(app: &AppHandle) {
    let Ok(bans) = with_db(app, |db| db.bans()) else { return };
    let json = serde_json::to_string(&bans).unwrap_or_default();
    for label in ["ytm", "panel"] {
        if let Some(w) = app.get_webview(label) {
            let _ = w.eval(format!("window.__ytmdSetBans && window.__ytmdSetBans({json})"));
        }
    }
}

#[tauri::command]
fn get_bans(app: AppHandle) -> Result<stats::Bans, String> {
    with_db(&app, |db| db.bans())
}

/// Ban whatever is playing now, and skip it.
#[tauri::command]
fn ban_current_song(app: AppHandle) -> Result<(), String> {
    let p = app.state::<AppState>().player.lock().unwrap().clone();
    if p.video_id.is_empty() {
        return Err("nothing is playing".into());
    }
    with_db(&app, |db| db.ban_song(&p.video_id, &p.title, &p.artist))?;
    broadcast_bans(&app);
    run_action(&app, Action::Next);
    Ok(())
}

/// Ban an artist (the panel passes one of the current song's credited artists), and skip.
#[tauri::command]
fn ban_artist(app: AppHandle, name: String) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("no artist".into());
    }
    with_db(&app, |db| db.ban_artist(&name))?;
    broadcast_bans(&app);
    run_action(&app, Action::Next);
    Ok(())
}

#[tauri::command]
fn unban_song(app: AppHandle, video_id: String) -> Result<(), String> {
    with_db(&app, |db| db.unban_song(&video_id))?;
    broadcast_bans(&app);
    Ok(())
}

#[tauri::command]
fn unban_artist(app: AppHandle, name: String) -> Result<(), String> {
    with_db(&app, |db| db.unban_artist(&name))?;
    broadcast_bans(&app);
    Ok(())
}

// ---------- equalizer ----------

#[tauri::command]
fn get_eq(app: AppHandle) -> EqState {
    app.state::<AppState>().eq.lock().unwrap().clone()
}

/// From the panel: apply to the page right away; `save` is set once a slider is let go.
#[tauri::command]
fn set_eq(app: AppHandle, webview: Webview, mut eq: EqState, save: bool) -> Result<(), String> {
    if webview.label() != "panel" {
        return Err("not allowed".into());
    }
    eq.gains.resize(EQ_BANDS, 0.0);
    for g in &mut eq.gains {
        *g = if g.is_finite() { g.clamp(-EQ_MAX_DB, EQ_MAX_DB) } else { 0.0 };
    }
    if let Some(w) = ytm_webview(&app) {
        let json = serde_json::to_string(&eq).unwrap_or_default();
        let _ = w.eval(format!("window.__ytmdSetEq && window.__ytmdSetEq({json})"));
    }
    if save {
        save_json(data_dir(&app).join("eq.json"), &eq);
    }
    *app.state::<AppState>().eq.lock().unwrap() = eq;
    Ok(())
}

// ---------- liked songs ----------

/// Tell the panel the liked list changed so it reloads it.
fn notify_likes(app: &AppHandle) {
    if let Some(w) = app.get_webview("panel") {
        let _ = w.eval("window.__panelLikesChanged && window.__panelLikesChanged()");
    }
}

#[tauri::command]
fn get_likes(app: AppHandle) -> Result<stats::Likes, String> {
    with_db(&app, |db| db.likes())
}

/// Panel's sync button: the YouTube Music page reads the playlist and sends it back via save_likes.
#[tauri::command]
fn sync_likes(app: AppHandle) -> Result<(), String> {
    let w = ytm_webview(&app).ok_or("YouTube Music isn't loaded")?;
    w.eval("window.__ytmdSyncLikes && window.__ytmdSyncLikes()").map_err(|e| e.to_string())
}

/// Sync progress from the YouTube Music page, passed on to the panel.
#[tauri::command]
fn likes_sync_status(app: AppHandle, webview: Webview, status: serde_json::Value) {
    if webview.label() != "ytm" {
        return;
    }
    if let Some(w) = app.get_webview("panel") {
        let _ = w.eval(format!("window.__panelLikesSync && window.__panelLikesSync({status})"));
    }
}

#[tauri::command]
fn save_likes(app: AppHandle, webview: Webview, songs: Vec<stats::LikedSong>) -> Result<(), String> {
    if webview.label() != "ytm" {
        return Err("not allowed".into());
    }
    match app.state::<AppState>().stats.lock().unwrap().as_mut() {
        Some(db) => db.replace_likes(&songs).map_err(|e| e.to_string())?,
        None => return Err("stats database unavailable".into()),
    }
    notify_likes(&app);
    Ok(())
}

/// 👍 in the player.
#[tauri::command]
fn like_song(app: AppHandle, song: stats::LikedSong) -> Result<(), String> {
    with_db(&app, |db| db.like(&song))?;
    notify_likes(&app);
    Ok(())
}

/// 👍 taken back in the player.
#[tauri::command]
fn unlike_song(app: AppHandle, video_id: String) -> Result<(), String> {
    with_db(&app, |db| db.unlike(&video_id))?;
    notify_likes(&app);
    Ok(())
}

/// 👎 in the player bans the song. YouTube Music skips it by itself, so no skip here.
#[tauri::command]
fn dislike_song(app: AppHandle, song: stats::LikedSong) -> Result<(), String> {
    with_db(&app, |db| db.ban_song(&song.video_id, &song.title, &song.artist))?;
    broadcast_bans(&app);
    Ok(())
}

/// Play a song from the panel (e.g. the Liked tab), inside the Liked Music playlist.
#[tauri::command]
fn play_song(app: AppHandle, video_id: String) -> Result<(), String> {
    let w = ytm_webview(&app).ok_or("YouTube Music isn't loaded")?;
    let id = serde_json::to_string(&video_id).unwrap_or_default();
    w.eval(format!("window.__ytmdPlay && window.__ytmdPlay({id}, 'LM')")).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct StatsView {
    ram_mb: u64,
    ram_history: Vec<(u64, u64)>,
    uptime_secs: u64,
    now: PlayerState,
    #[serde(flatten)]
    summary: stats::Summary,
}

#[tauri::command]
async fn get_stats(app: AppHandle) -> Result<StatsView, String> {
    let ram_mb = cached_memory_mb(&app);
    let s = app.state::<AppState>();
    let ram_history = s.memory.lock().unwrap().history.iter().copied().collect();
    let now = s.player.lock().unwrap().clone();
    let summary = match s.stats.lock().unwrap().as_ref() {
        Some(db) => db.summary(&now.video_id).map_err(|e| e.to_string())?,
        None => stats::Summary::default(),
    };
    Ok(StatsView { ram_mb, ram_history, uptime_secs: s.started.elapsed().as_secs(), now, summary })
}

// ---------- windows & tray ----------

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("YTM Player Settings")
        .inner_size(460.0, 580.0)
        .resizable(false)
        .additional_browser_args(&browser_args())
        .build();
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Play / Pause", true, None::<&str>)?;
    let next = MenuItem::with_id(app, "next", "Next", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "prev", "Previous", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(app, &[&show, &sep()?, &toggle, &next, &prev, &sep()?, &settings, &quit])?;

    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("YTM Player")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "toggle" => run_action(app, Action::Toggle),
            "next" => run_action(app, Action::Next),
            "prev" => run_action(app, Action::Prev),
            "settings" => open_settings(app),
            "quit" => {
                persist_player(app);
                save_window_state(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(window_state_flags())
                .with_denylist(&["settings"])
                .build(),
        )
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let action = app
                        .state::<AppState>()
                        .shortcuts
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(sc, _)| sc == shortcut)
                        .map(|(_, a)| *a);
                    if let Some(a) = action {
                        run_action(app, a);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            report_state,
            get_restore,
            get_settings,
            save_settings,
            suspend_shortcuts,
            get_memory_mb,
            get_panel_ui,
            toggle_panel,
            get_stats,
            get_bans,
            ban_current_song,
            ban_artist,
            unban_song,
            unban_artist,
            get_eq,
            set_eq,
            get_likes,
            sync_likes,
            likes_sync_status,
            save_likes,
            like_song,
            unlike_song,
            dislike_song,
            play_song
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let dir = data_dir(&handle);
            let settings: Settings = load_json(dir.join("settings.json"));
            let player: PlayerState = load_json(dir.join("player.json"));
            let ui: UiState = load_json(dir.join("ui.json"));
            let mut eq: EqState = load_json(dir.join("eq.json"));
            eq.gains.resize(EQ_BANDS, 0.0);
            let db = stats::Stats::open(&dir.join("stats.db"))
                .map_err(|e| eprintln!("stats db: {e}"))
                .ok();

            let start_url = if player.video_id.is_empty() {
                Url::parse(HOME_URL).unwrap()
            } else {
                watch_url(&player)
            };
            let restore = (!player.video_id.is_empty()).then(|| Restore {
                video_id: player.video_id.clone(),
                time: player.time,
                autoplay: false,
            });

            app.manage(AppState {
                settings: Mutex::new(settings.clone()),
                shortcuts: Mutex::new(Vec::new()),
                player: Mutex::new(PlayerState { playing: false, ..player }),
                last_persist: Mutex::new(Instant::now()),
                restore: Mutex::new(restore),
                reload_pending: Mutex::new(false),
                last_reload: Mutex::new(None),
                stats: Mutex::new(db),
                ui: Mutex::new(ui),
                eq: Mutex::new(eq),
                started: Instant::now(),
                slide_gen: Default::default(),
                memory: Mutex::new(MemoryCache { sys: sysinfo::System::new(), at: None, mb: 0, history: Default::default() }),
            });

            if let Err(e) = apply_shortcuts(&handle, &settings) {
                eprintln!("shortcut error: {e}");
            }

            // Created hidden so it doesn't flash at the default size before the saved one is applied.
            let main = WindowBuilder::new(app, "main")
                .title("YouTube Music")
                .inner_size(1280.0 + PANEL_OPEN_W, 820.0)
                // Roughly the smallest size where YouTube Music, its queue and the panel all fit comfortably.
                .min_inner_size(1000.0 + PANEL_OPEN_W, 820.0)
                // Shows for a moment between the webviews while resizing; match their dark theme.
                .background_color(WINDOW_BG)
                .visible(false)
                .build()?;
            let ytm = WebviewBuilder::new("ytm", WebviewUrl::External(start_url))
                .initialization_script(INJECT_JS)
                .background_color(WINDOW_BG)
                .additional_browser_args(&browser_args());
            let panel = WebviewBuilder::new("panel", WebviewUrl::App("panel.html".into()))
                .background_color(WINDOW_BG)
                .additional_browser_args(&browser_args());
            // Real positions are set by layout() once the window size is known.
            main.add_child(ytm, LogicalPosition::new(0.0, 0.0), LogicalSize::new(1280.0, 820.0))?;
            main.add_child(panel, LogicalPosition::new(1280.0, 0.0), LogicalSize::new(PANEL_OPEN_W, 820.0))?;
            let _ = main.restore_state(window_state_flags());
            layout(&handle);
            main.show()?;
            if let Some(w) = ytm_webview(&handle) {
                let _ = w.set_focus();
            }
            #[cfg(windows)]
            thumbbar::install(&main);

            build_tray(&handle)?;
            show_song(&handle, &handle.state::<AppState>().player.lock().unwrap().clone());
            start_watchdog(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    save_window_state(window.app_handle());
                    let _ = window.hide();
                }
                // Also save when you click away, so a PC restart keeps the latest size/position.
                WindowEvent::Focused(false) => save_window_state(window.app_handle()),
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => layout(window.app_handle()),
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building app");

    app.run(|app, event| {
        if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
            persist_player(app);
            save_window_state(app);
        }
    });
}
