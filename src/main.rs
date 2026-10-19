#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! RoomTune desktop UI: a short setup screen, then one EQ graph with room correction, manual
//! bands, and live level meters. Playback runs system-wide in the engine set up by RoomTune.
use anyhow::{Result, ensure};
use eframe::egui::{self, Align, Align2, Color32, FontFamily, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};
use room_tune::{apo::{self, Operation, Status}, audio::{self, Devices, Monitor, Tap}, dsp::{MicCalibration, log_grid, noisy_ranges}, engine,
    eq::{self, Band, EqConfig, Kind}, platform, profile::{Profile, atomic_write, data_dir, endpoint_guid}};
use serde::{Deserialize, Serialize};
use std::{panic::AssertUnwindSafe, path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

const INK: Color32 = Color32::from_rgb(28, 36, 33);
const MUTED: Color32 = Color32::from_rgb(110, 120, 114);
const FAINT: Color32 = Color32::from_rgb(226, 231, 225);
const GREEN: Color32 = Color32::from_rgb(24, 112, 84);
const PAPER: Color32 = Color32::from_rgb(244, 246, 242);
const ERROR: Color32 = Color32::from_rgb(176, 58, 40);
const AMBER: Color32 = Color32::from_rgb(196, 140, 30);
const MEASURED: Color32 = Color32::from_rgb(150, 158, 152);
const ACCENT: Color32 = Color32::from_rgb(214, 108, 36);
const LIVE_DELAY: Duration = Duration::from_millis(200);

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--helper") { std::process::exit(apo::helper_main(&args[1..])); }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("RoomTune").with_inner_size([1120.0, 720.0]).with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };
    let demo = args.iter().any(|a| a == "--demo");
    let main_screen = args.iter().any(|a| a == "--main");
    eframe::run_native("RoomTune", options, Box::new(move |cc| {
        let mut app = App::new(&cc.egui_ctx);
        if main_screen || demo { app.screen = Screen::Main; }
        if demo { app.toggle_demo(); }
        Ok(Box::new(app))
    }))
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings { output_id: String, input_id: String, input_channel: usize, level_db: f64, setup_done: bool }

impl Default for Settings {
    fn default() -> Self { Self { output_id: String::new(), input_id: String::new(), input_channel: 0, level_db: -30.0, setup_done: false } }
}

/// The UI edits one manual EQ for both speakers (room correction stays per speaker).
fn linked(mut eq: EqConfig) -> EqConfig { eq.manual.link = true; eq.manual.sync(); eq }

fn settings_path() -> PathBuf { data_dir().join("settings.json") }
fn eq_path() -> PathBuf { data_dir().join("eq.roomtune") }
fn last_measurement_path() -> PathBuf { data_dir().join("last-measurement.roomtune") }

enum Msg {
    Devices(Result<Devices>),
    Progress(String, f32),
    Calibrated(Box<Result<Profile>>, Option<String>, bool),
    Playback(Result<String>),
    Setup(Result<bool>),
    Panicked,
}

#[derive(Clone)]
struct Notifier { tx: mpsc::Sender<Msg>, ctx: egui::Context }

impl Notifier {
    fn send(&self, msg: Msg) { let _ = self.tx.send(msg); self.ctx.request_repaint(); }
    fn progress(&self, text: &str, fraction: f32) { self.send(Msg::Progress(text.into(), fraction)); }
}

#[derive(PartialEq)]
enum Job { Devices, Calibrating, Playback, Setup }

#[derive(PartialEq, Clone, Copy)]
enum Screen { Setup, Main }

#[derive(Default)]
struct Engine { installed: bool, on_output: bool, status: Option<Status> }

impl Engine {
    fn ready(&self) -> bool { self.installed && self.on_output }
    fn active(&self) -> bool { self.ready() && self.status == Some(Status::Enabled) }
}

/// A level meter with smoothed fall-off and peak hold, in dBFS.
struct Meter { monitor: Monitor, key: (String, Tap), level: [f32; 2], hold: [f32; 2], held: [Instant; 2] }

const FLOOR_DB: f32 = -60.0;

impl Meter {
    fn new(id: String, tap: Tap) -> Self {
        Self { monitor: audio::monitor(id.clone(), tap), key: (id, tap), level: [FLOOR_DB; 2], hold: [FLOOR_DB; 2], held: [Instant::now(); 2] }
    }

    fn update(&mut self, dt: f32) {
        for slot in 0..2 {
            let db = (20.0 * self.monitor.levels.take(slot).max(1e-6).log10()).max(FLOOR_DB);
            self.level[slot] = db.max(self.level[slot] - 36.0 * dt);
            if db >= self.hold[slot] { self.hold[slot] = db; self.held[slot] = Instant::now(); }
            else if self.held[slot].elapsed() > Duration::from_millis(1200) { self.hold[slot] = (self.hold[slot] - 24.0 * dt).max(self.level[slot]); }
        }
    }

    fn failed(&self) -> bool { self.monitor.levels.failed.load(Ordering::Relaxed) }
}

struct App {
    settings: Settings,
    screen: Screen,
    devices: Devices,
    mic: MicCalibration,
    ready: bool,
    eq: EqConfig,
    eq_before_demo: Option<EqConfig>,
    selected: Option<usize>,
    status: String,
    status_error: bool,
    progress: Option<f32>,
    job: Option<Job>,
    cancel: Arc<AtomicBool>,
    engine: Engine,
    engine_checked: Instant,
    devices_checked: Instant,
    restart_needed: bool,
    continue_after_setup: bool,
    calibrating_open: bool,
    dirty_at: Option<Instant>,
    error: Option<String>,
    out_meter: Option<Meter>,
    in_meter: Option<Meter>,
    last_frame: Instant,
    notifier: Notifier,
    rx: mpsc::Receiver<Msg>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        style(ctx);
        let (tx, rx) = mpsc::channel();
        let settings: Settings = std::fs::read_to_string(settings_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let eq = linked(EqConfig::load(&eq_path()).ok()
            .or_else(|| Profile::load(&last_measurement_path()).ok().map(EqConfig::from_profile)).unwrap_or_default());
        let screen = if settings.setup_done { Screen::Main } else { Screen::Setup };
        let mut app = Self {
            settings, screen, devices: Devices::default(), mic: MicCalibration::default(), ready: false, eq, eq_before_demo: None,
            selected: None, status: String::new(), status_error: false, progress: None, job: None,
            cancel: Arc::new(AtomicBool::new(false)), engine: Engine::default(), engine_checked: Instant::now(), devices_checked: Instant::now(),
            restart_needed: false, continue_after_setup: false, calibrating_open: false, dirty_at: None, error: None,
            out_meter: None, in_meter: None, last_frame: Instant::now(), notifier: Notifier { tx, ctx: ctx.clone() }, rx,
        };
        app.refresh_engine();
        app.refresh_devices();
        app
    }

    fn busy(&self) -> bool { self.job.is_some() }
    fn calibrating(&self) -> bool { self.job == Some(Job::Calibrating) }
    fn demo(&self) -> bool { self.eq.is_demo() }

    fn spawn(&mut self, job: Job, work: impl FnOnce(&Notifier) -> Msg + Send + 'static) {
        self.job = Some(job);
        let notifier = self.notifier.clone();
        thread::spawn(move || {
            let msg = std::panic::catch_unwind(AssertUnwindSafe(|| work(&notifier))).unwrap_or(Msg::Panicked);
            notifier.send(msg);
        });
    }

    fn set_status(&mut self, text: impl Into<String>) { self.status = text.into(); self.status_error = false; }

    fn fail(&mut self, e: impl std::fmt::Display) {
        self.status = e.to_string(); self.status_error = true;
        self.error = Some(self.status.clone());
    }

    fn save_settings(&self) {
        if let Ok(text) = serde_json::to_string_pretty(&self.settings) { let _ = atomic_write(&settings_path(), text.as_bytes()); }
    }

    fn output(&self) -> Option<&audio::DeviceInfo> { self.devices.outputs.iter().find(|d| d.id == self.settings.output_id) }
    fn input(&self) -> Option<&audio::DeviceInfo> { self.devices.inputs.iter().find(|d| d.id == self.settings.input_id) }

    /// The endpoint the EQ is written for: the calibrated output, else the selected one.
    fn target_id(&self) -> Option<String> {
        Some(self.eq.output_id.clone()).filter(|id| !id.is_empty()).or_else(|| self.output().map(|d| d.id.clone()))
    }

    fn target_name(&self) -> String {
        if self.eq.output_name.is_empty() { self.output().map_or("your speakers".into(), |d| d.name.clone()) } else { self.eq.output_name.clone() }
    }

    fn refresh_engine(&mut self) {
        let installed = engine::installed();
        let on_output = installed && self.target_id().and_then(|id| endpoint_guid(&id).ok()).is_some_and(|g| engine::enabled_on(&g));
        self.engine = Engine { installed, on_output, status: installed.then(apo::status) };
        self.engine_checked = Instant::now();
    }

    fn refresh_devices(&mut self) {
        self.devices_checked = Instant::now();
        // WASAPI enumeration uses COM; keep it off the UI thread, which winit initialises for OLE.
        self.spawn(Job::Devices, |_| Msg::Devices(audio::devices()));
    }

    /// Without a room measurement the EQ follows the selected output.
    fn follow_output(&mut self) {
        if self.eq.room.is_some() || self.demo() { return; }
        if let Some(d) = self.output().cloned() && d.id != self.eq.output_id {
            self.eq.output_id = d.id; self.eq.output_name = d.name;
            self.mark_dirty();
            self.refresh_engine();
        }
    }

    fn mark_dirty(&mut self) { self.dirty_at = Some(Instant::now()); }

    fn persist(&mut self) {
        if self.demo() { return; }
        if let Err(e) = self.eq.save(&eq_path()) { self.status = format!("Could not save your EQ: {e:#}"); self.status_error = true; }
    }

    /// Writes EQ changes straight into the running engine (the config folder is user-writable).
    fn apply_live(&mut self) {
        self.persist();
        if !self.engine.active() || self.demo() { return; }
        let Some(root) = apo::find_dir() else { return; };
        match apo::write_operation(&root, Operation::Apply, Some(&self.eq)) {
            Ok(()) => if self.status_error { self.set_status(""); },
            // Still busy after the write retries: try again shortly rather than bother the user.
            Err(e) if platform::is_access_denied(&e) => { self.dirty_at = Some(Instant::now()); self.set_status("Engine busy · retrying…"); }
            Err(e) => { self.status = format!("{e:#}"); self.status_error = true; }
        }
    }

    fn handle(&mut self, msg: Msg) {
        if !matches!(msg, Msg::Progress(..)) { self.job = None; self.progress = None; }
        match msg {
            Msg::Progress(text, fraction) => { self.progress = Some(fraction); self.set_status(text); }
            Msg::Devices(Ok(devices)) => {
                let pick = |wanted: &[&str], list: &[audio::DeviceInfo]| wanted.iter().find(|w| list.iter().any(|d| d.id == **w)).map(|w| w.to_string())
                    .or_else(|| list.iter().find(|d| d.is_default).or(list.first()).map(|d| d.id.clone())).unwrap_or_default();
                let output = pick(&[&self.settings.output_id, &self.eq.output_id], &devices.outputs);
                let input = pick(&[&self.settings.input_id], &devices.inputs);
                let changed = output != self.settings.output_id || input != self.settings.input_id;
                self.devices = devices;
                self.settings.output_id = output; self.settings.input_id = input;
                if self.input().is_none_or(|d| self.settings.input_channel >= d.channels as usize) { self.settings.input_channel = 0; }
                if changed { self.save_settings(); }
                self.follow_output();
            }
            Msg::Devices(Err(e)) => self.fail(format!("Could not list audio devices: {e:#}")),
            Msg::Calibrated(result, warning, resume) => {
                match *result {
                    Ok(profile) => {
                        if let Some(backup) = self.eq_before_demo.take() { self.eq = backup; }
                        let manual = std::mem::take(&mut self.eq.manual);
                        self.eq = EqConfig { manual, ..EqConfig::from_profile(profile) };
                        self.persist();
                        self.calibrating_open = false;
                        let cuts = self.eq.filters(false).len().min(99);
                        self.set_status(format!("Room measured · {cuts} correction filters on the left speaker."));
                        if resume { self.refresh_engine(); self.apply(); }
                    }
                    Err(e) => if self.cancel.load(Ordering::Relaxed) { self.set_status("Calibration cancelled."); } else { self.fail(format!("{e:#}")); },
                }
                if let Some(warning) = warning { self.fail(warning); }
            }
            Msg::Playback(Ok(text)) => self.set_status(text),
            Msg::Playback(Err(e)) => self.fail(format!("{e:#}")),
            Msg::Setup(Ok(restart)) => {
                self.restart_needed = restart;
                self.refresh_engine();
                self.set_status(if restart { "Engine installed. Restart Windows to start correcting playback." } else { "Playback engine ready." });
                if self.continue_after_setup { self.finish_setup(); }
                if !restart && self.has_eq() && !self.demo() { self.apply(); }
            }
            Msg::Setup(Err(e)) => self.fail(format!("{e:#}")),
            Msg::Panicked => self.fail("An internal error stopped the operation."),
        }
        if !self.busy() { self.refresh_engine(); }
    }

    fn has_eq(&self) -> bool { !self.eq.filters(false).is_empty() || !self.eq.filters(true).is_empty() }

    fn finish_setup(&mut self) {
        self.settings.setup_done = true;
        self.save_settings();
        self.screen = Screen::Main;
        self.follow_output();
    }

    fn calibrate(&mut self) {
        let (Some(output), Some(input)) = (self.output().cloned(), self.input().cloned()) else { return; };
        let request = audio::Request { output, input, input_channel: self.settings.input_channel, level_db: self.settings.level_db, mic: self.mic.clone() };
        if let Err(e) = request.validate() { self.fail(e); return; }
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let active = self.engine.active();
        self.progress = Some(0.0);
        self.set_status("Preparing measurement…");
        self.spawn(Job::Calibrating, move |n| {
            let mut bypassed = false;
            let result = (|| -> Result<Profile> {
                if active {
                    n.progress("Bypassing the EQ for measurement…", 0.0);
                    apo::execute(Operation::Bypass, None)?;
                    bypassed = true;
                    thread::sleep(Duration::from_millis(600));
                }
                let profile = audio::calibrate(&request, &cancel, &|text, fraction| n.progress(text, fraction))?;
                profile.save(&last_measurement_path())?;
                Ok(profile)
            })();
            // On success the UI applies the new EQ; otherwise put the previous one back.
            let warning = (bypassed && result.is_err()).then(|| apo::execute(Operation::Restore, None).err()).flatten()
                .map(|e| format!("Your EQ could not be switched back on ({e:#}). Use the EQ switch."));
            Msg::Calibrated(Box::new(result), warning, bypassed)
        });
    }

    fn start_setup(&mut self) {
        let Some(target) = self.target_id() else { self.fail("Choose your speakers first."); return; };
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let needs_install = !self.engine.installed;
        self.progress = Some(0.0);
        self.spawn(Job::Setup, move |n| Msg::Setup((|| {
            let installer = if needs_install {
                n.progress("Downloading engine…", 0.0);
                Some(engine::download(&|p| n.progress("Downloading engine…", p * 0.6), &|| cancel.load(Ordering::Relaxed))?)
            } else { None };
            n.progress("Installing · approve the Windows permission prompt", 0.7);
            let installer = installer.map_or("-".into(), |p| p.to_string_lossy().into_owned());
            Ok(apo::run_helper(vec!["setup".into(), installer, target])? == 2)
        })()));
    }

    fn apply(&mut self) {
        if let Err(e) = self.eq.validate(true) { self.fail(e); return; }
        let eq = self.eq.clone();
        self.persist();
        self.spawn(Job::Playback, move |_| Msg::Playback((|| {
            ensure!(audio::devices()?.outputs.iter().any(|d| d.id == eq.output_id), "{} is disconnected", eq.output_name);
            apo::execute(Operation::Apply, Some(&eq))?;
            Ok("EQ on · edits apply instantly, and RoomTune can be closed.".into())
        })()));
    }

    fn bypass(&mut self) {
        self.spawn(Job::Playback, |_| Msg::Playback(apo::execute(Operation::Bypass, None).map(|()| "EQ off.".into())));
    }

    fn toggle_demo(&mut self) {
        match self.eq_before_demo.take() {
            Some(previous) => { self.eq = previous; self.set_status(""); }
            None => {
                self.eq_before_demo = Some(std::mem::replace(&mut self.eq, EqConfig::from_profile(Profile::demo())));
                self.set_status("Demo · synthetic room data, not applied to playback.");
            }
        }
        self.selected = None;
        self.dirty_at = None;
    }

    fn load_mic(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("Microphone calibration", &["txt", "cal", "csv", "frd"]).add_filter("All files", &["*"]).pick_file() else { return; };
        match MicCalibration::load(&path) { Ok(mic) => self.mic = mic, Err(e) => self.fail(format!("{e:#}")) }
    }

    fn save(&mut self, export: bool) {
        let dialog = rfd::FileDialog::new();
        let dialog = if export { dialog.set_file_name("roomtune-apo.txt").add_filter("Equalizer APO configuration", &["txt"]) }
            else { dialog.set_file_name("my-eq.roomtune").add_filter("RoomTune EQ", &["roomtune"]) };
        let Some(path) = dialog.save_file() else { return; };
        let result = if export { apo::export(&self.eq).and_then(|text| atomic_write(&path, text.as_bytes())) } else { self.eq.save(&path) };
        match result { Ok(()) => self.set_status(format!("Saved {}", path.display())), Err(e) => self.fail(format!("{e:#}")) }
    }

    fn open(&mut self) {
        let Some(path) = rfd::FileDialog::new().set_directory(data_dir()).add_filter("RoomTune EQ or profile", &["roomtune", "json"]).pick_file() else { return; };
        match EqConfig::load(&path) {
            Ok(eq) => {
                self.eq_before_demo = None;
                self.eq = linked(eq);
                self.selected = None;
                self.set_status(format!("Opened {}.", path.file_name().unwrap_or_default().to_string_lossy()));
                self.follow_output();
                self.mark_dirty();
                self.refresh_engine();
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
    }

    /// Keeps the output meter on the selected speakers, and the input meter on only where it
    /// helps (setup and calibration), so the microphone is not held open in normal use.
    fn sync_meters(&mut self, dt: f32) {
        let want_out = self.output().map(|d| (d.id.clone(), Tap::Output));
        let mic_visible = self.screen == Screen::Setup || self.calibrating_open;
        let want_in = self.input().filter(|_| mic_visible).map(|d| (d.id.clone(), Tap::Input(self.settings.input_channel)));
        for (meter, want) in [(&mut self.out_meter, want_out), (&mut self.in_meter, want_in)] {
            if meter.as_ref().map(|m| &m.key) != want.as_ref() { *meter = want.map(|(id, tap)| Meter::new(id, tap)); }
            if let Some(m) = meter { m.update(dt); }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() { self.handle(msg); }
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.2);
        self.last_frame = Instant::now();
        if !self.busy() && self.engine_checked.elapsed() > Duration::from_secs(2) { self.refresh_engine(); }
        if !self.busy() && self.screen == Screen::Setup && self.devices_checked.elapsed() > Duration::from_secs(4) { self.refresh_devices(); }
        if let Some(t) = self.dirty_at && t.elapsed() >= LIVE_DELAY && !self.busy() { self.dirty_at = None; self.apply_live(); }
        self.sync_meters(dt);
        ctx.request_repaint_after(if self.out_meter.is_some() || self.in_meter.is_some() { Duration::from_millis(33) } else { Duration::from_secs(1) });
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.busy() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.cancel.store(true, Ordering::Relaxed);
                self.set_status("Finishing the current step. Close again when it completes.");
            } else if self.dirty_at.take().is_some() { self.apply_live(); }
        }
        match self.screen {
            Screen::Setup => self.setup_screen(ctx),
            Screen::Main => self.main_screen(ctx),
        }
        if self.calibrating_open { self.calibrate_dialog(ctx); }
        self.error_dialog(ctx);
    }
}

// ---------- small widgets ----------

fn semi(text: impl Into<String>, size: f32) -> RichText { RichText::new(text).family(FontFamily::Name("semi".into())).size(size).color(INK) }
fn muted(text: impl Into<String>) -> RichText { RichText::new(text).size(12.5).color(MUTED) }

fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(semi(text, 14.0).color(Color32::WHITE)).fill(GREEN).corner_radius(6.0).min_size(vec2(0.0, 32.0))
}

fn chip(selected: bool, text: impl Into<String>) -> egui::Button<'static> {
    let text = text.into();
    let (fill, color) = if selected { (Color32::from_rgb(219, 236, 227), GREEN) } else { (Color32::TRANSPARENT, INK) };
    egui::Button::new(RichText::new(text).color(color)).fill(fill).corner_radius(14.0).min_size(vec2(0.0, 26.0))
}

/// iOS-style switch.
fn switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if response.clicked() { *on = !*on; response.mark_changed(); }
    let t = ui.ctx().animate_bool(response.id, *on);
    let enabled = ui.is_enabled();
    let track = if enabled { Color32::from_rgb(200, 206, 201).lerp_to_gamma(GREEN, t) } else { FAINT };
    ui.painter().rect_filled(rect, 11.0, track);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    ui.painter().circle_filled(pos2(x, rect.center().y), 8.5, Color32::WHITE);
    response
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn meter_color(db: f32) -> Color32 { if db > -3.0 { ERROR } else if db > -12.0 { AMBER } else { GREEN } }

/// One meter bar from -60 to 0 dBFS, filling upward (vertical) or rightward.
fn meter_bar(painter: &egui::Painter, rect: Rect, level: f32, hold: f32, vertical: bool) {
    painter.rect_filled(rect, 2.0, FAINT);
    let frac = |db: f32| ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    let part = |f: f32| if vertical { Rect::from_min_max(pos2(rect.left(), rect.bottom() - rect.height() * f), rect.max) }
        else { Rect::from_min_max(rect.min, pos2(rect.left() + rect.width() * f, rect.bottom())) };
    if level > FLOOR_DB { painter.rect_filled(part(frac(level)), 2.0, meter_color(level)); }
    if hold > FLOOR_DB + 1.0 {
        let f = frac(hold);
        let line = if vertical { let y = rect.bottom() - rect.height() * f; [pos2(rect.left(), y), pos2(rect.right(), y)] }
            else { let x = rect.left() + rect.width() * f; [pos2(x, rect.top()), pos2(x, rect.bottom())] };
        painter.line_segment(line, Stroke::new(2.0, meter_color(hold)));
    }
}

fn horizontal_meter(ui: &mut egui::Ui, meter: Option<&Meter>, stereo: bool, hint: &str) {
    let rows = if stereo { 2 } else { 1 };
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, rows as f32 * 8.0 + (rows - 1) as f32 * 3.0), Sense::hover());
    for row in 0..rows {
        let r = Rect::from_min_size(rect.min + vec2(0.0, row as f32 * 11.0), vec2(width, 8.0));
        let (level, hold) = meter.map_or((FLOOR_DB, FLOOR_DB), |m| (m.level[row], m.hold[row]));
        meter_bar(ui.painter(), r, level, hold, false);
    }
    let text = match meter {
        Some(m) if m.failed() => "Meter unavailable for this device".to_string(),
        Some(m) if m.hold[0].max(m.hold[1]) > FLOOR_DB + 1.0 => format!("{:.0} dBFS peak", m.hold[0].max(m.hold[1])),
        _ => hint.to_string(),
    };
    ui.label(muted(text).size(11.5));
}

fn card(ui: &mut egui::Ui, number: &str, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(Color32::WHITE).corner_radius(10.0).inner_margin(egui::Margin::symmetric(18, 14))
        .stroke(Stroke::new(1.0, FAINT)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.horizontal(|ui| { ui.label(semi(number, 13.0).color(GREEN)); ui.label(semi(title, 15.0)); });
                ui.add_space(4.0);
                add(ui);
            });
        });
    ui.add_space(10.0);
}

fn device_combo(ui: &mut egui::Ui, id: &str, selected: &mut String, list: &[audio::DeviceInfo], empty: &str) -> bool {
    let current = list.iter().find(|d| d.id == *selected).map_or(empty.to_string(), |d| d.name.clone());
    let before = selected.clone();
    egui::ComboBox::from_id_salt(id).width(ui.available_width()).selected_text(current).show_ui(ui, |ui| {
        for d in list { ui.selectable_value(selected, d.id.clone(), format!("{}  ·  {} ch  ·  {} Hz", d.name, d.channels, d.sample_rate)); }
    });
    *selected != before
}

fn short_hz(f: f64) -> String { if f >= 1000.0 { format!("{:.1}k", f / 1000.0).replace(".0k", "k") } else { format!("{f:.0}") } }

// ---------- screens ----------

impl App {
    fn setup_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(PAPER).inner_margin(24)).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| ui.vertical_centered(|ui| {
                ui.set_max_width(520.0);
                ui.add_space(((ui.available_height() - 600.0) / 2.0).max(8.0));
                ui.label(semi("RoomTune", 30.0));
                ui.label(muted("Room correction and EQ for everything you play.").size(14.0));
                ui.add_space(20.0);
                let idle = !self.busy() || self.job == Some(Job::Devices);

                card(ui, "1", "Speakers", |ui| {
                    ui.add_enabled_ui(idle, |ui| {
                        if device_combo(ui, "output", &mut self.settings.output_id, &self.devices.outputs, "No stereo output found") {
                            self.save_settings(); self.follow_output(); self.refresh_engine();
                        }
                    });
                    ui.add_space(6.0);
                    horizontal_meter(ui, self.out_meter.as_ref(), true, "Play something to check the level");
                });

                card(ui, "2", "Measurement microphone", |ui| {
                    ui.add_enabled_ui(idle, |ui| {
                        let channels = self.input().map_or(1, |d| d.channels as usize);
                        ui.horizontal(|ui| {
                            let width = ui.available_width();
                            ui.allocate_ui(vec2(width - 110.0, 24.0), |ui| {
                                if device_combo(ui, "input", &mut self.settings.input_id, &self.devices.inputs, "No microphone found") {
                                    self.settings.input_channel = 0; self.save_settings();
                                }
                            });
                            let before = self.settings.input_channel;
                            egui::ComboBox::from_id_salt("channel").width(100.0).selected_text(format!("Input {}", self.settings.input_channel + 1)).show_ui(ui, |ui| {
                                for c in 0..channels { ui.selectable_value(&mut self.settings.input_channel, c, format!("Input {}", c + 1)); }
                            });
                            if before != self.settings.input_channel { self.save_settings(); }
                        });
                    });
                    ui.add_space(6.0);
                    horizontal_meter(ui, self.in_meter.as_ref(), false, "Speak or clap to check the input");
                    ui.label(muted("Needed only for room calibration. A measurement mic gives the best result."));
                });

                card(ui, "3", "Playback engine", |ui| {
                    let (color, text) = if self.engine.ready() {
                        (GREEN, if self.restart_needed { "Installed · restart Windows to activate it".to_string() } else { format!("Ready on {}", self.target_name()) })
                    } else if self.engine.installed { (AMBER, "Installed · not yet enabled for these speakers".to_string()) }
                    else { (MUTED, "Not installed".to_string()) };
                    ui.horizontal(|ui| { dot(ui, color); ui.label(RichText::new(text).color(INK)); });
                    if !self.engine.ready() {
                        let what = if self.engine.installed { "RoomTune enables it for your speakers" }
                            else { "RoomTune installs the open-source Equalizer APO engine (12 MB, verified) and enables it for your speakers" };
                        ui.label(muted(format!("{what}, so your EQ works in every app, even with RoomTune closed. Windows asks for permission once.")));
                    }
                    ui.label(muted("ASIO and exclusive-mode apps bypass the engine."));
                });

                ui.add_space(6.0);
                if self.job == Some(Job::Setup) {
                    ui.add(egui::ProgressBar::new(self.progress.unwrap_or(0.0)).desired_width(ui.available_width()).text(self.status.clone()));
                } else {
                    let label = if self.engine.ready() { "Continue" } else if self.engine.installed { "Enable engine & continue" } else { "Install engine & continue" };
                    let can = idle && self.output().is_some();
                    if ui.add_enabled(can, primary(label).min_size(vec2(ui.available_width(), 40.0))).clicked() {
                        if self.engine.ready() { self.finish_setup(); } else { self.continue_after_setup = true; self.start_setup(); }
                    }
                    if !self.engine.ready() && ui.add_enabled(idle, egui::Link::new(muted("Skip for now"))).clicked() { self.finish_setup(); }
                    if self.status_error { ui.label(RichText::new(&self.status).size(12.5).color(ERROR)); }
                }
            }));
        });
    }

    fn main_screen(&mut self, ctx: &egui::Context) {
        let idle = !self.busy();
        egui::TopBottomPanel::top("bar").frame(egui::Frame::new().fill(PAPER).inner_margin(egui::Margin { left: 18, right: 14, top: 10, bottom: 6 })).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(semi("RoomTune", 18.0));
                ui.add_space(12.0);
                let name = self.target_name();
                let (color, text) = if self.demo() { (AMBER, "Demo data".to_string()) }
                    else if !self.engine.installed { (MUTED, "Playback engine not set up · menu › Devices & engine".to_string()) }
                    else if !self.engine.on_output { (AMBER, format!("Engine not enabled for {name} · menu › Devices & engine")) }
                    else if self.restart_needed { (AMBER, "Restart Windows to activate the engine".to_string()) }
                    else if self.engine.active() { (GREEN, format!("EQ on · {name}")) }
                    else { (MUTED, format!("EQ off · {name}")) };
                dot(ui, color);
                ui.label(RichText::new(text).color(INK));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.menu(ui, idle);
                    ui.add_space(10.0);
                    let mut on = self.engine.active();
                    let can_toggle = idle && self.engine.ready() && !self.demo() && !self.restart_needed;
                    if ui.add_enabled_ui(can_toggle, |ui| switch(ui, &mut on)).inner.changed() { if on { self.apply(); } else { self.bypass(); } }
                    ui.label(semi("EQ", 14.0));
                });
            });
        });

        egui::TopBottomPanel::bottom("editor").frame(egui::Frame::new().fill(PAPER).inner_margin(egui::Margin { left: 18, right: 18, top: 6, bottom: 10 })).show(ctx, |ui| {
            let before = (self.eq.manual.clone(), self.eq.room_enabled);
            self.toolbar(ui, idle);
            ui.add_space(4.0);
            self.band_editor(ui);
            if (self.eq.manual.clone(), self.eq.room_enabled) != before { self.eq.manual.sync(); self.mark_dirty(); }
            if !self.status.is_empty() && !self.calibrating() {
                ui.add_space(2.0);
                ui.label(RichText::new(&self.status).size(12.0).color(if self.status_error { ERROR } else { MUTED }));
            }
        });

        egui::SidePanel::right("meter").exact_width(44.0).resizable(false).show_separator_line(false)
            .frame(egui::Frame::new().fill(PAPER).inner_margin(egui::Margin { left: 4, right: 14, top: 8, bottom: 8 })).show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let painter = ui.painter();
                painter.text(pos2(rect.center().x, rect.top()), Align2::CENTER_TOP, "OUT", FontId::proportional(10.0), MUTED);
                let bars = Rect::from_min_max(rect.min + vec2(2.0, 18.0), rect.max - vec2(2.0, 18.0));
                let half = (bars.width() - 4.0) / 2.0;
                for slot in 0..2 {
                    let r = Rect::from_min_size(bars.min + vec2(slot as f32 * (half + 4.0), 0.0), vec2(half, bars.height()));
                    let (level, hold) = self.out_meter.as_ref().map_or((FLOOR_DB, FLOOR_DB), |m| (m.level[slot], m.hold[slot]));
                    meter_bar(painter, r, level, hold, true);
                    painter.text(pos2(r.center().x, rect.bottom()), Align2::CENTER_BOTTOM, ["L", "R"][slot], FontId::proportional(10.0), MUTED);
                }
            });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(PAPER).inner_margin(egui::Margin { left: 18, right: 4, top: 4, bottom: 4 })).show(ctx, |ui| {
            let editable = !self.demo() && !self.calibrating();
            let before = self.eq.manual.clone();
            let selected = editable.then_some(&mut self.selected);
            plot(ui, &mut self.eq, selected);
            if self.eq.manual != before { self.mark_dirty(); }
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.add_enabled_ui(idle, |ui| ui.menu_button(semi("•••", 14.0), |ui| {
            ui.set_min_width(200.0);
            if ui.button("Devices & engine…").clicked() { self.screen = Screen::Setup; self.refresh_devices(); ui.close(); }
            ui.separator();
            if ui.button("Open EQ…").clicked() { ui.close(); self.open(); }
            if ui.add_enabled(!self.demo(), egui::Button::new("Save EQ…")).clicked() { ui.close(); self.save(false); }
            if ui.add_enabled(!self.demo() && self.target_id().is_some(), egui::Button::new("Export for Equalizer APO…")).clicked() { ui.close(); self.save(true); }
            ui.separator();
            if ui.button(if self.demo() { "Exit demo" } else { "Show demo" }).clicked() { self.toggle_demo(); ui.close(); }
        }));
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.horizontal(|ui| {
            if let Some(room) = &self.eq.room {
                let (l, r) = (room.left.filters.len(), room.right.filters.len());
                let label = if l == r { format!("Room correction · {l} cut{} per speaker", if l == 1 { "" } else { "s" }) }
                    else { format!("Room correction · L {l} / R {r} cuts") };
                let tip = {
                    let mut t = format!("Measured {} with {}", room.output_name, room.calibration_name);
                    for (side, m) in [("Left", &room.left), ("Right", &room.right)] {
                        t += &format!("\n\n{side} · mic peak {:.1} dBFS · noise {:.1} dBFS", m.peak_db, m.noise_db);
                        for f in &m.filters { t += &format!("\n{:>6.1} Hz   {:+.1} dB   Q {:.2}", f.frequency, f.gain, f.q); }
                        let notes = m.fit_notes();
                        if m.filters.is_empty() && notes.is_empty() { t += "\nNo resonances needed cutting."; }
                        for note in notes { t += &format!("\n· {note}"); }
                    }
                    t
                };
                let enabled = self.eq.room_enabled;
                if ui.add_enabled(!self.demo(), chip(enabled, label)).on_hover_text(tip).clicked() { self.eq.room_enabled = !enabled; }
            } else {
                ui.label(muted("No room measurement yet"));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can = idle && self.input().is_some() && self.output().is_some();
                if ui.add_enabled(can, egui::Button::new(if self.eq.room.is_some() { "Recalibrate room…" } else { "Calibrate room…" })
                    .corner_radius(6.0).min_size(vec2(0.0, 28.0))).clicked() {
                    self.calibrating_open = true; self.ready = false;
                }
            });
        });
    }

    fn band_editor(&mut self, ui: &mut egui::Ui) {
        let editable = !self.demo() && !self.calibrating();
        ui.add_enabled_ui(editable, |ui| ui.horizontal(|ui| {
            let bands = self.eq.manual.bands_mut(false);
            if self.selected.is_some_and(|i| i >= bands.len()) { self.selected = None; }
            for (i, b) in bands.iter().enumerate() {
                let text = format!("{}  {}{}", i + 1, short_hz(b.frequency), if b.enabled { "" } else { " · off" });
                let response = ui.add(chip(self.selected == Some(i), text));
                if response.clicked() { self.selected = if self.selected == Some(i) { None } else { Some(i) }; }
            }
            if bands.len() < eq::MAX_BANDS {
                ui.menu_button("+ Band", |ui| {
                    for (kind, f, q) in [(Kind::Peak, 1000.0, 1.0), (Kind::LowShelf, 120.0, 0.71), (Kind::HighShelf, 6000.0, 0.71)] {
                        if ui.button(kind.label()).clicked() {
                            bands.push(Band { kind, ..Band::peak(f, 0.0, q) });
                            self.selected = Some(bands.len() - 1);
                            ui.close();
                        }
                    }
                });
            }
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            match self.selected.and_then(|i| bands.get_mut(i).map(|b| (i, b))) {
                Some((i, b)) => {
                    egui::ComboBox::from_id_salt("kind").width(96.0).selected_text(b.kind.label()).show_ui(ui, |ui| {
                        for k in Kind::ALL { ui.selectable_value(&mut b.kind, k, k.label()); }
                    });
                    let speed = b.frequency * 0.004;
                    ui.add(egui::DragValue::new(&mut b.frequency).range(eq::FREQ_RANGE).speed(speed).fixed_decimals(0).suffix(" Hz"));
                    ui.add(egui::DragValue::new(&mut b.gain).range(eq::GAIN_RANGE).speed(0.05).fixed_decimals(1).suffix(" dB"));
                    ui.add(egui::DragValue::new(&mut b.q).range(eq::Q_RANGE).speed(0.01).fixed_decimals(2).prefix("Q "));
                    ui.add_space(4.0);
                    switch(ui, &mut b.enabled).on_hover_text("Band on/off");
                    ui.add_space(4.0);
                    let delete = ui.input(|inp| inp.key_pressed(egui::Key::Delete)) && !ui.ctx().wants_keyboard_input();
                    if ui.button("Remove").clicked() || delete { bands.remove(i); self.selected = None; }
                }
                None => { ui.label(muted(if bands.is_empty() { "Double-click the graph to add a band" } else { "Select a band, or drag its point on the graph · scroll on a point for Q" })); }
            }
        }));
    }

    fn calibrate_dialog(&mut self, ctx: &egui::Context) {
        let running = self.calibrating();
        let modal = egui::Modal::new(egui::Id::new("calibrate")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(semi("Calibrate your room", 18.0));
            ui.label(muted("Each speaker plays a 10-second sweep. RoomTune then cuts bass resonances between 30 and 300 Hz."));
            ui.add_space(10.0);
            ui.add_enabled_ui(!running, |ui| {
                ui.label(muted("Microphone"));
                let channels = self.input().map_or(1, |d| d.channels as usize);
                ui.horizontal(|ui| {
                    let width = ui.available_width();
                    ui.allocate_ui(vec2(width - 110.0, 24.0), |ui| {
                        if device_combo(ui, "cal-input", &mut self.settings.input_id, &self.devices.inputs, "No microphone found") {
                            self.settings.input_channel = 0; self.save_settings();
                        }
                    });
                    egui::ComboBox::from_id_salt("cal-channel").width(100.0).selected_text(format!("Input {}", self.settings.input_channel + 1)).show_ui(ui, |ui| {
                        for c in 0..channels { ui.selectable_value(&mut self.settings.input_channel, c, format!("Input {}", c + 1)); }
                    });
                });
                ui.add_space(4.0);
                horizontal_meter(ui, self.in_meter.as_ref(), false, "Clap to check the input");
                ui.add_space(6.0);
                ui.label(muted("Sweep level · start with moderate speaker volume"));
                ui.spacing_mut().slider_width = ui.available_width() - 90.0;
                if ui.add(egui::Slider::new(&mut self.settings.level_db, -42.0..=-18.0).step_by(3.0).fixed_decimals(0).suffix(" dBFS")).changed() { self.save_settings(); }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(muted(format!("Mic calibration: {}", self.mic.name)));
                    if ui.link(if self.mic.name == MicCalibration::default().name { "Load…" } else { "Change…" }).clicked() { self.load_mic(); }
                });
                ui.add_space(6.0);
                ui.checkbox(&mut self.ready, "Mic at ear height in the listening seat, room quiet, other EQ and mic enhancements off");
            });
            ui.add_space(12.0);
            if running {
                ui.add(egui::ProgressBar::new(self.progress.unwrap_or(0.0)).desired_width(ui.available_width()).text(self.status.clone()));
                ui.add_space(6.0);
                if ui.button("Cancel").clicked() { self.cancel.store(true, Ordering::Relaxed); }
            } else {
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.ready && self.input().is_some(), primary("Start calibration")).clicked() { self.calibrate(); }
                    if ui.button("Close").clicked() { self.calibrating_open = false; }
                });
            }
        });
        if modal.should_close() && !running { self.calibrating_open = false; }
    }

    fn error_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error.clone() else { return; };
        let modal = egui::Modal::new(egui::Id::new("error")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(semi("Something needs attention", 16.0));
            ui.add_space(6.0);
            ui.label(message);
            ui.add_space(10.0);
            ui.button("OK").clicked()
        });
        if modal.inner || modal.should_close() { self.error = None; }
    }
}

fn round_frequency(f: f64) -> f64 {
    let step = 10_f64.powf(f.log10().floor() - 2.0);
    ((f / step).round() * step).clamp(20.0, 20_000.0)
}

fn draw_curve(painter: &egui::Painter, points: Vec<Pos2>, stroke: Stroke, dashed: bool) {
    if dashed { painter.extend(egui::Shape::dashed_line(&points, stroke, 6.0, 4.0)); } else { painter.add(egui::Shape::line(points, stroke)); }
}

/// The response graph for both speakers: left solid, right dashed. With `selected`, the shared
/// manual bands are draggable.
fn plot(ui: &mut egui::Ui, eq: &mut EqConfig, selected: Option<&mut Option<usize>>) {
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
    let outer = response.rect;
    painter.rect_filled(outer, 10.0, Color32::WHITE);
    painter.rect_stroke(outer, 10.0, Stroke::new(1.0, FAINT), egui::StrokeKind::Inside);
    let rect = Rect::from_min_max(outer.min + vec2(40.0, 34.0), outer.max - vec2(16.0, 26.0));
    let measured = eq.room.is_some();
    let max_gain = eq.manual.bands(false).iter().map(|b| b.gain.abs()).fold(0.0, f64::max);
    let range = if measured || max_gain > 10.5 { 18.0 } else { 12.0 };
    let x = |f: f64| rect.left() + ((f / 20.0).ln() / 1000_f64.ln()) as f32 * rect.width();
    let y = |db: f64| rect.top() + ((range - db.clamp(-range, range)) / (2.0 * range)) as f32 * rect.height();
    let freq_at = |px: f32| 20.0 * 1000_f64.powf(((px - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64);
    let db_at = |py: f32| range - ((py - rect.top()) / rect.height()) as f64 * 2.0 * range;
    let small = FontId::proportional(11.0);
    let grid = Stroke::new(1.0, Color32::from_rgb(238, 241, 237));

    painter.rect_filled(Rect::from_x_y_ranges(x(30.0)..=x(300.0), rect.y_range()), 0.0, Color32::from_rgb(246, 249, 244));
    // Spans where either speaker's sweep was too close to background noise to trust (or cut).
    let noisy: Vec<(f64, f64)> = [false, true].into_iter().filter_map(|right| eq.measurement(right))
        .flat_map(|m| noisy_ranges(&m.frequencies, &m.snr)).collect();
    for &(lo, hi) in &noisy {
        painter.rect_filled(Rect::from_x_y_ranges(x(lo / 1.015)..=x(hi * 1.015), rect.y_range()), 0.0, Color32::from_rgba_unmultiplied(214, 160, 40, 26));
    }
    let step = if range > 12.0 { 6 } else { 3 };
    for db in (-(range as i32)..=range as i32).step_by(step) {
        let stroke = if db == 0 { Stroke::new(1.0, Color32::from_rgb(214, 220, 214)) } else { grid };
        painter.line_segment([pos2(rect.left(), y(db as f64)), pos2(rect.right(), y(db as f64))], stroke);
        painter.text(pos2(rect.left() - 8.0, y(db as f64)), Align2::RIGHT_CENTER, format!("{db:+}").replace("+0", "0"), small.clone(), MUTED);
    }
    for f in [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000] {
        painter.line_segment([pos2(x(f as f64), rect.top()), pos2(x(f as f64), rect.bottom())], grid);
        painter.text(pos2(x(f as f64), rect.bottom() + 6.0), Align2::CENTER_TOP, short_hz(f as f64), small.clone(), MUTED);
    }
    let mut legend_x = outer.left() + 16.0;
    let mut legend = |color: Color32, text: &str, dashed: bool| {
        let p = pos2(legend_x, outer.top() + 16.0);
        draw_curve(&painter, vec![p, p + vec2(16.0, 0.0)], Stroke::new(2.0, color), dashed);
        let galley = painter.layout_no_wrap(text.into(), small.clone(), MUTED);
        let width = galley.size().x;
        painter.galley(p + vec2(22.0, -galley.size().y / 2.0), galley, MUTED);
        legend_x += 36.0 + width;
    };
    if measured { legend(MEASURED, "Measured", false); legend(GREEN, "With EQ", false); }
    legend(ACCENT, "EQ", false);
    if measured { legend(MUTED, "Left", false); legend(MUTED, "Right", true); }
    if !noisy.is_empty() {
        let p = pos2(legend_x, outer.top() + 16.0);
        painter.rect_filled(Rect::from_center_size(p + vec2(8.0, 0.0), vec2(16.0, 10.0)), 2.0, Color32::from_rgba_unmultiplied(214, 160, 40, 70));
        painter.text(p + vec2(22.0, 0.0), Align2::LEFT_CENTER, "Low SNR (no cuts)", small.clone(), MUTED);
    }
    if eq.is_demo() { painter.text(pos2(outer.right() - 16.0, outer.top() + 16.0), Align2::RIGHT_CENTER, "DEMO DATA", small.clone(), AMBER); }

    // The shared manual EQ, filled; the full per-speaker EQ (room + manual) drawn on top.
    let freqs = log_grid(20.0, 20_000.0, 2_f64.powf(1.0 / 24.0));
    let manual_points: Vec<Pos2> = freqs.iter().map(|&f| pos2(x(f), y(eq.manual.bands(false).iter().map(|b| b.db(f)).sum()))).collect();
    let mut mesh = egui::Mesh::default();
    let fill = Color32::from_rgba_unmultiplied(214, 108, 36, 30);
    let base = y(0.0);
    for w in manual_points.windows(2) {
        let i = mesh.vertices.len() as u32;
        for p in [w[0], pos2(w[0].x, base), w[1], pos2(w[1].x, base)] { mesh.colored_vertex(p, fill); }
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
    for right in [false, true] {
        if let Some(m) = eq.measurement(right) {
            let line = |db: &dyn Fn(usize) -> f64| -> Vec<Pos2> { m.frequencies.iter().enumerate().map(|(i, &f)| pos2(x(f), y(db(i)))).collect() };
            draw_curve(&painter, line(&|i| m.response[i]), Stroke::new(1.3, MEASURED), right);
            draw_curve(&painter, line(&|i| m.response[i] + eq.response(right, m.frequencies[i])), Stroke::new(2.0, GREEN), right);
        }
    }
    let room_differs = eq.room_enabled && eq.room.as_ref().is_some_and(|p| !p.left.filters.is_empty() || !p.right.filters.is_empty());
    for right in if room_differs { vec![false, true] } else { vec![false] } {
        draw_curve(&painter, freqs.iter().map(|&f| pos2(x(f), y(eq.response(right, f)))).collect(), Stroke::new(2.0, ACCENT), right);
    }

    let mut dragging = false;
    let mut readout = None;
    if let Some(selected) = selected {
        let mut changed = false;
        let bands = eq.manual.bands_mut(false);
        for (i, b) in bands.iter_mut().enumerate() {
            let center = pos2(x(b.frequency), y(b.gain));
            let handle = ui.interact(Rect::from_center_size(center, vec2(22.0, 22.0)), response.id.with(("band", i)), Sense::click_and_drag())
                .on_hover_cursor(egui::CursorIcon::Grab);
            if handle.clicked() || handle.drag_started() { *selected = Some(i); }
            if handle.dragged() && let Some(p) = handle.interact_pointer_pos() {
                b.frequency = round_frequency(freq_at(p.x));
                b.gain = (db_at(p.y).clamp(*eq::GAIN_RANGE.start(), *eq::GAIN_RANGE.end()) * 10.0).round() / 10.0;
                changed = true; dragging = true;
            }
            if handle.hovered() || handle.dragged() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 { b.q = ((b.q * (1.0 + scroll as f64 * 0.004)) * 100.0).round().clamp(10.0, 1000.0) / 100.0; changed = true; }
                readout = Some(format!("{}  ·  {} Hz  ·  {:+.1} dB  ·  Q {:.2}", b.kind.label(), short_hz(b.frequency), b.gain, b.q));
            }
            let is_selected = *selected == Some(i);
            let radius = if is_selected { 9.0 } else { 7.5 };
            painter.circle(center, radius, if is_selected { ACCENT } else if b.enabled { Color32::WHITE } else { FAINT }, Stroke::new(1.8, ACCENT));
            painter.text(center, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(10.0), if is_selected { Color32::WHITE } else { INK });
        }
        if response.double_clicked() && bands.len() < eq::MAX_BANDS && let Some(p) = response.interact_pointer_pos().filter(|p| rect.contains(*p)) {
            bands.push(Band::peak(round_frequency(freq_at(p.x)), (db_at(p.y).clamp(-12.0, 12.0) * 10.0).round() / 10.0, 1.0));
            *selected = Some(bands.len() - 1);
            changed = true;
        }
        if response.clicked() && !changed { *selected = None; }
        if changed { eq.manual.sync(); }
    }

    if readout.is_none() && !dragging && let Some(pointer) = response.hover_pos().filter(|p| rect.contains(*p)) {
        let f = freq_at(pointer.x);
        painter.line_segment([pos2(pointer.x, rect.top()), pos2(pointer.x, rect.bottom())], Stroke::new(1.0, FAINT));
        let (l, r) = (eq.response(false, f), eq.response(true, f));
        let mut text = if (l - r).abs() < 0.05 { format!("{} Hz  ·  EQ {l:+.1} dB", short_hz(f)) } else { format!("{} Hz  ·  EQ L {l:+.1} / R {r:+.1} dB", short_hz(f)) };
        let with_eq = |right: bool| eq.measurement(right).map(|m| {
            let i = (0..m.frequencies.len()).min_by(|&a, &b| (m.frequencies[a] / f).ln().abs().total_cmp(&(m.frequencies[b] / f).ln().abs())).unwrap_or(0);
            m.response[i] + eq.response(right, m.frequencies[i])
        });
        if let (Some(l), Some(r)) = (with_eq(false), with_eq(true)) { text += &format!("  ·  with EQ L {l:+.1} / R {r:+.1} dB"); }
        readout = Some(text);
    }
    if let Some(text) = readout {
        let galley = painter.layout_no_wrap(text, small, INK);
        let pos = pos2(outer.right() - 16.0 - galley.size().x, outer.top() + 16.0 - galley.size().y / 2.0);
        painter.rect_filled(Rect::from_min_size(pos, galley.size()).expand2(vec2(8.0, 4.0)), 6.0, PAPER);
        painter.galley(pos, galley, INK);
    }
}

fn style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut semi_family = fallback.clone();
    // Prefer the native Windows UI fonts when present; egui's bundled fonts are the fallback.
    if let Some(windir) = std::env::var_os("WINDIR").map(PathBuf::from) {
        let fonts_dir = windir.join("Fonts");
        if let Ok(bytes) = std::fs::read(fonts_dir.join("segoeui.ttf")) {
            fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
            semi_family.insert(0, "segoe".into());
        }
        if let Ok(bytes) = std::fs::read(fonts_dir.join("seguisb.ttf")) {
            fonts.font_data.insert("segoe-semibold".into(), Arc::new(egui::FontData::from_owned(bytes)));
            semi_family.insert(0, "segoe-semibold".into());
        }
    }
    fonts.families.insert(FontFamily::Name("semi".into()), semi_family);
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::Theme::Light);
    ctx.style_mut_of(egui::Theme::Light, |style| {
        style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(12.0, 4.0);
        style.spacing.interact_size.y = 26.0;
        let v = &mut style.visuals;
        v.panel_fill = PAPER;
        v.window_fill = Color32::WHITE;
        v.window_corner_radius = 12.0.into();
        v.window_stroke = Stroke::new(1.0, FAINT);
        v.widgets.noninteractive.fg_stroke.color = INK;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, FAINT);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] { w.corner_radius = 6.0.into(); }
        // bg_fill draws slider rails and checkbox boxes; keep it visible on white dialogs.
        v.widgets.inactive.bg_fill = Color32::from_rgb(228, 233, 227);
        v.widgets.inactive.weak_bg_fill = Color32::WHITE;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(214, 221, 214));
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(236, 242, 237);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(190, 204, 194));
        v.selection.bg_fill = Color32::from_rgb(210, 232, 220);
        v.selection.stroke.color = GREEN;
    });
}
