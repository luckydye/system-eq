#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! RoomTune desktop UI: a short setup screen, then one EQ graph with room correction, manual
//! bands, and live level meters. Playback runs system-wide in the engine set up by RoomTune.
use anyhow::{Result, ensure};
use eframe::egui::{self, Align, Align2, Color32, FontFamily, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use room_tune::{apo::{self, Operation, Status}, audio::{self, Devices, Monitor, Tap}, dsp::{Measurement, MicCalibration, log_grid, noisy_ranges}, engine,
    eq::{self, Band, EqConfig, Kind}, platform, profile::{Profile, atomic_write, data_dir, endpoint_guid}};
use serde::{Deserialize, Serialize};
use std::{panic::AssertUnwindSafe, path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

// Studio palette: dark chassis, darker wells for displays, bright traces.
// One continuous background for the main window (the graph sits directly on it); wells are
// slightly darker insets for meters and readouts; PANEL is for dialogs and setup cards.
const BG: Color32 = Color32::from_rgb(14, 15, 17);
const PANEL: Color32 = Color32::from_rgb(28, 31, 35);
const WELL: Color32 = Color32::from_rgb(8, 9, 10);
const EDGE: Color32 = Color32::from_rgb(46, 51, 58);
const GRID: Color32 = Color32::from_rgb(26, 29, 33);
const GRID_MAJOR: Color32 = Color32::from_rgb(40, 45, 51);
const TEXT: Color32 = Color32::from_rgb(214, 219, 225);
const DIM: Color32 = Color32::from_rgb(130, 139, 150);
const FAINT: Color32 = Color32::from_rgb(76, 83, 92);
const AMBER: Color32 = Color32::from_rgb(255, 166, 64);
const CYAN: Color32 = Color32::from_rgb(70, 214, 205);
const MEASURED: Color32 = Color32::from_rgb(150, 160, 172);
const LED_GREEN: Color32 = Color32::from_rgb(64, 220, 112);
const LED_YELLOW: Color32 = Color32::from_rgb(240, 204, 64);
const LED_RED: Color32 = Color32::from_rgb(255, 76, 62);
const BAND_COLORS: [Color32; 8] = [
    Color32::from_rgb(96, 168, 255), Color32::from_rgb(186, 128, 255), Color32::from_rgb(255, 110, 160), Color32::from_rgb(120, 222, 120),
    Color32::from_rgb(255, 214, 80), Color32::from_rgb(80, 220, 230), Color32::from_rgb(255, 136, 90), Color32::from_rgb(200, 200, 210),
];
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
struct Settings { output_id: String, input_id: String, input_channel: usize, level_db: f64, setup_done: bool, positions: usize }

impl Default for Settings {
    fn default() -> Self { Self { output_id: String::new(), input_id: String::new(), input_channel: 0, level_db: -30.0, setup_done: false, positions: 5 } }
}

/// Where to put the microphone for each measurement position (listener's view).
const POSITIONS: [(&str, Vec2); 7] = [
    ("Listening position, ear height", Vec2::new(0.0, 0.0)),
    ("30 cm to the left", Vec2::new(-1.0, 0.0)),
    ("30 cm to the right", Vec2::new(1.0, 0.0)),
    ("20 cm forward, toward the speakers", Vec2::new(0.0, -0.7)),
    ("20 cm back", Vec2::new(0.0, 0.7)),
    ("Center, 15 cm higher", Vec2::new(0.45, -0.45)),
    ("Center, 15 cm lower", Vec2::new(-0.45, 0.45)),
];

/// The UI edits one manual EQ for both speakers (room correction stays per speaker).
fn linked(mut eq: EqConfig) -> EqConfig { eq.manual.link = true; eq.manual.sync(); eq }

fn settings_path() -> PathBuf { data_dir().join("settings.json") }
fn eq_path() -> PathBuf { data_dir().join("eq.roomtune") }
fn last_measurement_path() -> PathBuf { data_dir().join("last-measurement.roomtune") }

enum Msg {
    Devices(Result<Devices>),
    Progress(String, f32),
    /// One microphone position measured (left, right); the flag says the EQ was bypassed for it.
    Position(Box<Result<(Measurement, Measurement)>>, bool),
    Restored(Result<()>),
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
    /// Positions measured so far in the current room measurement.
    session: Vec<(Measurement, Measurement)>,
    /// The EQ was bypassed for this measurement session and must be re-applied afterwards.
    session_bypassed: bool,
    plot_view: PlotView,
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
            restart_needed: false, continue_after_setup: false, calibrating_open: false, session: vec![], session_bypassed: false, plot_view: PlotView::default(), dirty_at: None, error: None,
            out_meter: None, in_meter: None, last_frame: Instant::now(), notifier: Notifier { tx, ctx: ctx.clone() }, rx,
        };
        app.refresh_engine();
        app.refresh_devices();
        // Measurements from before full-range fitting get the current correction automatically.
        if let Some(room) = app.eq.room.as_mut().filter(|r| r.needs_refit()) {
            room.refit();
            app.set_status("Re-fitted your room measurement with full-range correction.");
            app.mark_dirty();
        }
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
            Msg::Position(result, bypassed) => {
                self.session_bypassed |= bypassed;
                match *result {
                    Ok(position) => {
                        self.session.push(position);
                        if self.session.len() >= self.settings.positions { self.finish_session(); }
                        else { self.set_status(format!("Position {} done · move the mic to position {}.", self.session.len(), self.session.len() + 1)); }
                    }
                    Err(e) => if self.cancel.load(Ordering::Relaxed) { self.set_status("Measurement cancelled. Measure this position again or close."); } else { self.fail(format!("{e:#}")); },
                }
            }
            Msg::Restored(result) => if let Err(e) = result { self.fail(format!("Your EQ could not be switched back on ({e:#}). Use the EQ button.")); },
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
        // The EQ stays bypassed for the whole session and is re-applied when it ends.
        let bypass = self.engine.active() && !self.session_bypassed;
        let position = self.session.len() + 1;
        self.progress = Some(0.0);
        self.set_status(format!("Position {position} · preparing…"));
        self.spawn(Job::Calibrating, move |n| {
            let mut bypassed = false;
            let result = (|| -> Result<(Measurement, Measurement)> {
                if bypass {
                    n.progress("Bypassing the EQ for measurement…", 0.0);
                    apo::execute(Operation::Bypass, None)?;
                    bypassed = true;
                    thread::sleep(Duration::from_millis(600));
                }
                audio::measure_position(&request, &cancel, &|text, fraction| n.progress(&format!("Position {position} · {text}"), fraction))
            })();
            Msg::Position(Box::new(result), bypassed)
        });
    }

    /// Averages the measured positions into the room correction and applies it.
    fn finish_session(&mut self) {
        let (Some(output), Some(input)) = (self.output().cloned(), self.input().cloned()) else { return; };
        let request = audio::Request { output, input, input_channel: self.settings.input_channel, level_db: self.settings.level_db, mic: self.mic.clone() };
        let positions = std::mem::take(&mut self.session);
        match audio::build_profile(&request, &positions) {
            Ok(profile) => {
                let _ = profile.save(&last_measurement_path());
                self.set_room(profile, &format!("{} position{}", positions.len(), if positions.len() == 1 { "" } else { "s" }));
                self.calibrating_open = false;
                if std::mem::take(&mut self.session_bypassed) { self.refresh_engine(); self.apply(); }
            }
            Err(e) => self.fail(format!("{e:#}")),
        }
    }

    /// Ends an unfinished session: discards it and switches a bypassed EQ back on.
    fn abandon_session(&mut self) {
        self.session.clear();
        if std::mem::take(&mut self.session_bypassed) {
            self.spawn(Job::Playback, |_| Msg::Restored(apo::execute(Operation::Restore, None)));
        }
    }

    /// Installs a room measurement as the room correction, keeping the manual EQ.
    fn set_room(&mut self, profile: Profile, source: &str) {
        if let Some(backup) = self.eq_before_demo.take() { self.eq = backup; }
        let manual = std::mem::take(&mut self.eq.manual);
        self.eq = linked(EqConfig { manual, ..EqConfig::from_profile(profile) });
        self.persist();
        let (l, r) = self.eq.room.as_ref().map_or((0, 0), |p| (p.left.filters.len(), p.right.filters.len()));
        self.set_status(format!("Room measured ({source}) · {l} filters left, {r} right."));
        self.mark_dirty();
        self.refresh_engine();
    }

    fn import_soundid(&mut self) {
        let start = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Sonarworks\SoundID Reference\Sonarworks Projects"));
        let mut dialog = rfd::FileDialog::new().add_filter("SoundID Reference project", &["swproj"]);
        if let Some(dir) = start.filter(|d| d.is_dir()) { dialog = dialog.set_directory(dir); }
        let Some(path) = dialog.pick_file() else { return; };
        let Some(output) = self.output().cloned() else { self.fail("Choose your speaker output first."); return; };
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let result = room_tune::soundid::load(&path).and_then(|(l, r)| Profile::from_positions(output.id.clone(), output.name.clone(),
            "SoundID Reference measurement".into(), name.clone(), &[(l, r)]));
        match result {
            Ok(profile) => self.set_room(profile, &format!("imported from {name}")),
            Err(e) => self.fail(format!("{e:#}")),
        }
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

// ---------- studio widgets ----------

fn semi(text: impl Into<String>, size: f32) -> RichText { RichText::new(text).family(FontFamily::Name("semi".into())).size(size).color(TEXT) }
fn caps(text: &str) -> RichText { semi(text.to_uppercase(), 10.5).color(DIM) }
fn dim(text: impl Into<String>) -> RichText { RichText::new(text).size(12.0).color(DIM) }
fn mono(text: impl Into<String>, size: f32) -> RichText { RichText::new(text).monospace().size(size).color(TEXT) }

/// Vertical gradient fill, the basis of the raised hardware look.
fn gradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    for (p, c) in [(rect.left_top(), top), (rect.right_top(), top), (rect.left_bottom(), bottom), (rect.right_bottom(), bottom)] { mesh.colored_vertex(p, c); }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

fn led(painter: &egui::Painter, center: Pos2, color: Color32, on: bool) {
    if on {
        painter.circle_filled(center, 7.0, color.gamma_multiply(0.18));
        painter.circle_filled(center, 3.5, color);
        painter.circle_filled(center + vec2(-1.0, -1.0), 1.2, Color32::from_white_alpha(140));
    } else {
        painter.circle(center, 3.5, Color32::from_rgb(34, 37, 42), Stroke::new(1.0, Color32::from_rgb(10, 11, 13)));
    }
}

/// Recessed display well with an inner shadow.
fn well(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 3.0, WELL);
    painter.rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_rgb(6, 7, 8)), StrokeKind::Inside);
    painter.line_segment([rect.left_bottom() + vec2(3.0, -0.5), rect.right_bottom() + vec2(-3.0, -0.5)], Stroke::new(1.0, Color32::from_white_alpha(10)));
}

/// A raised hardware button. `led_color` adds a status LED (lit when `on`); `on` also latches it.
fn hw_button(ui: &mut egui::Ui, text: &str, size: Vec2, led_color: Option<Color32>, on: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let painter = ui.painter();
        let pressed = response.is_pointer_button_down_on();
        let (top, bottom) = if pressed { (Color32::from_rgb(30, 33, 37), Color32::from_rgb(36, 40, 45)) }
            else if response.hovered() && enabled { (Color32::from_rgb(64, 70, 78), Color32::from_rgb(46, 50, 57)) }
            else { (Color32::from_rgb(54, 59, 66), Color32::from_rgb(39, 43, 48)) };
        gradient(painter, rect, top, bottom);
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_rgb(10, 11, 13)), StrokeKind::Outside);
        if !pressed { painter.line_segment([rect.left_top() + vec2(2.0, 0.5), rect.right_top() + vec2(-2.0, 0.5)], Stroke::new(1.0, Color32::from_white_alpha(22))); }
        let mut center = rect.center();
        if let Some(color) = led_color {
            led(painter, pos2(rect.left() + 12.0, rect.center().y), color, on && enabled);
            center.x += 8.0;
        }
        painter.text(center, Align2::CENTER_CENTER, text, FontId::new(12.0, FontFamily::Name("semi".into())), if enabled { TEXT } else { FAINT });
    }
    response
}

/// The amber call-to-action button.
fn accent_button(ui: &mut egui::Ui, text: &str, size: Vec2) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let painter = ui.painter();
        let (top, bottom) = if !enabled { (Color32::from_rgb(70, 60, 46), Color32::from_rgb(58, 50, 40)) }
            else if response.is_pointer_button_down_on() { (Color32::from_rgb(214, 132, 40), Color32::from_rgb(232, 146, 50)) }
            else if response.hovered() { (Color32::from_rgb(255, 190, 100), Color32::from_rgb(244, 150, 52)) }
            else { (Color32::from_rgb(255, 178, 84), Color32::from_rgb(232, 140, 44)) };
        gradient(painter, rect, top, bottom);
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::from_rgb(10, 11, 13)), StrokeKind::Outside);
        painter.text(rect.center(), Align2::CENTER_CENTER, text.to_uppercase(), FontId::new(12.5, FontFamily::Name("semi".into())),
            if enabled { Color32::from_rgb(24, 18, 10) } else { Color32::from_rgb(120, 104, 84) });
    }
    response
}

fn zone_color(db: f32) -> Color32 { if db > -3.0 { LED_RED } else if db > -12.0 { LED_YELLOW } else { LED_GREEN } }

/// Segmented LED meter, -60..0 dBFS, with peak hold.
fn led_meter(painter: &egui::Painter, rect: Rect, level: f32, hold: f32, vertical: bool) {
    painter.rect_filled(rect, 1.0, WELL);
    let length = if vertical { rect.height() } else { rect.width() };
    let count = ((length - 2.0) / 4.0).floor().max(1.0) as usize;
    let step = (length - 2.0) / count as f32;
    for i in 0..count {
        let (lo, hi) = (FLOOR_DB - FLOOR_DB * i as f32 / count as f32, FLOOR_DB - FLOOR_DB * (i + 1) as f32 / count as f32);
        let lit = level >= hi - 0.01 || (level > lo && level < hi);
        let held = hold > lo && hold <= hi && hold > FLOOR_DB + 1.0;
        let color = zone_color(lo);
        let fill = if lit || held { color } else { color.gamma_multiply(0.10) };
        let a = 1.0 + i as f32 * step;
        let seg = if vertical { Rect::from_min_max(pos2(rect.left() + 1.0, rect.bottom() - a - step + 1.0), pos2(rect.right() - 1.0, rect.bottom() - a)) }
            else { Rect::from_min_max(pos2(rect.left() + a, rect.top() + 1.0), pos2(rect.left() + a + step - 1.0, rect.bottom() - 1.0)) };
        painter.rect_filled(seg, 0.0, fill);
    }
}

fn horizontal_meter(ui: &mut egui::Ui, meter: Option<&Meter>, stereo: bool, hint: &str) {
    let rows = if stereo { 2 } else { 1 };
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, rows as f32 * 9.0 + (rows - 1) as f32 * 3.0 + 14.0), Sense::hover());
    let painter = ui.painter();
    let bar_width = width - 64.0;
    for row in 0..rows {
        let r = Rect::from_min_size(rect.min + vec2(0.0, row as f32 * 12.0), vec2(bar_width, 9.0));
        let (level, hold) = meter.map_or((FLOOR_DB, FLOOR_DB), |m| (m.level[row], m.hold[row]));
        led_meter(painter, r, level, hold, false);
    }
    for db in [-48, -36, -24, -12, -6, 0] {
        let x = rect.left() + bar_width * (1.0 - db as f32 / FLOOR_DB);
        painter.text(pos2(x, rect.bottom()), Align2::CENTER_BOTTOM, db.to_string(), FontId::monospace(9.0), FAINT);
    }
    let peak = meter.map_or(FLOOR_DB, |m| m.hold[0].max(m.hold[1]));
    let readout = match meter {
        Some(m) if m.failed() => "n/a".to_string(),
        _ if peak > FLOOR_DB + 1.0 => format!("{peak:+.1}"),
        _ => "-inf".to_string(),
    };
    let box_rect = Rect::from_min_size(pos2(rect.right() - 56.0, rect.top()), vec2(56.0, rows as f32 * 12.0 - 3.0));
    well(painter, box_rect);
    painter.text(box_rect.center(), Align2::CENTER_CENTER, readout, FontId::monospace(11.0), if peak > -3.0 { LED_RED } else { TEXT });
    if meter.is_none_or(|m| m.hold[0].max(m.hold[1]) <= FLOOR_DB + 1.0) { ui.label(dim(hint).size(11.0)); }
}

fn panel(ui: &mut egui::Ui, number: &str, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(PANEL).corner_radius(4.0).inner_margin(egui::Margin::symmetric(16, 12))
        .stroke(Stroke::new(1.0, EDGE)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                ui.horizontal(|ui| { ui.label(semi(number, 11.0).color(AMBER)); ui.label(caps(title).color(TEXT)); });
                ui.add_space(6.0);
                add(ui);
            });
        });
    ui.add_space(8.0);
}

fn device_combo(ui: &mut egui::Ui, id: &str, selected: &mut String, list: &[audio::DeviceInfo], empty: &str) -> bool {
    let current = list.iter().find(|d| d.id == *selected).map_or(empty.to_string(), |d| d.name.clone());
    let before = selected.clone();
    egui::ComboBox::from_id_salt(id).width(ui.available_width()).selected_text(current).show_ui(ui, |ui| {
        for d in list { ui.selectable_value(selected, d.id.clone(), format!("{}  ·  {} ch  ·  {} Hz", d.name, d.channels, d.sample_rate)); }
    });
    *selected != before
}

fn channel_combo(ui: &mut egui::Ui, id: &str, channel: &mut usize, channels: usize) -> bool {
    let before = *channel;
    egui::ComboBox::from_id_salt(id).width(92.0).selected_text(format!("IN {}", *channel + 1)).show_ui(ui, |ui| {
        for c in 0..channels { ui.selectable_value(channel, c, format!("Input {}", c + 1)); }
    });
    *channel != before
}

fn short_hz(f: f64) -> String { if f >= 1000.0 { format!("{:.1}k", f / 1000.0).replace(".0k", "k") } else { format!("{f:.0}") } }

fn band_color(i: usize) -> Color32 { BAND_COLORS[i % BAND_COLORS.len()] }

/// A labelled parameter field (caption above, monospace control in a dark well).
fn param<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(caps(label));
        ui.scope(|ui| {
            let v = ui.visuals_mut();
            v.widgets.inactive.weak_bg_fill = WELL;
            v.widgets.hovered.weak_bg_fill = Color32::from_rgb(18, 20, 23);
            v.widgets.active.weak_bg_fill = Color32::from_rgb(22, 25, 28);
            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
            add(ui)
        }).inner
    }).inner
}

// ---------- screens ----------

impl App {
    fn status_line(&self) -> (Color32, bool, String) {
        let name = self.target_name();
        if self.demo() { (LED_YELLOW, true, "DEMO DATA".into()) }
        else if !self.engine.installed { (LED_RED, false, "ENGINE NOT INSTALLED".into()) }
        else if !self.engine.on_output { (LED_YELLOW, true, format!("ENGINE NOT ENABLED · {name}")) }
        else if self.restart_needed { (LED_YELLOW, true, "RESTART WINDOWS".into()) }
        else if self.engine.active() { (LED_GREEN, true, format!("LIVE · {name}")) }
        else { (LED_GREEN, false, format!("BYPASSED · {name}")) }
    }

    fn setup_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(24)).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| ui.vertical_centered(|ui| {
                ui.set_max_width(540.0);
                ui.add_space(((ui.available_height() - 580.0) / 2.0).max(8.0));
                ui.label(semi("ROOMTUNE", 26.0));
                ui.label(dim("Room correction & system-wide EQ").size(13.0));
                ui.add_space(18.0);
                let idle = !self.busy() || self.job == Some(Job::Devices);

                panel(ui, "01", "Output", |ui| {
                    ui.add_enabled_ui(idle, |ui| {
                        if device_combo(ui, "output", &mut self.settings.output_id, &self.devices.outputs, "No stereo output found") {
                            self.save_settings(); self.follow_output(); self.refresh_engine();
                        }
                    });
                    ui.add_space(8.0);
                    horizontal_meter(ui, self.out_meter.as_ref(), true, "Play something to check the level");
                });

                panel(ui, "02", "Measurement microphone", |ui| {
                    ui.add_enabled_ui(idle, |ui| ui.horizontal(|ui| {
                        let channels = self.input().map_or(1, |d| d.channels as usize);
                        let width = ui.available_width();
                        ui.allocate_ui(vec2(width - 102.0, 24.0), |ui| {
                            if device_combo(ui, "input", &mut self.settings.input_id, &self.devices.inputs, "No microphone found") {
                                self.settings.input_channel = 0; self.save_settings();
                            }
                        });
                        if channel_combo(ui, "channel", &mut self.settings.input_channel, channels) { self.save_settings(); }
                    }));
                    ui.add_space(8.0);
                    horizontal_meter(ui, self.in_meter.as_ref(), false, "Speak or clap to check the input");
                    ui.label(dim("Only needed for room calibration. A measurement mic gives the best result."));
                });

                panel(ui, "03", "Playback engine", |ui| {
                    let (color, on, text) = if self.engine.ready() {
                        (LED_GREEN, true, if self.restart_needed { "Installed · restart Windows to activate".to_string() } else { format!("Ready on {}", self.target_name()) })
                    } else if self.engine.installed { (LED_YELLOW, true, "Installed · not enabled for this output".to_string()) }
                    else { (LED_RED, false, "Not installed".to_string()) };
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                        led(ui.painter(), rect.center(), color, on);
                        ui.label(RichText::new(text).color(TEXT));
                    });
                    if !self.engine.ready() {
                        let what = if self.engine.installed { "RoomTune enables Equalizer APO for this output" }
                            else { "RoomTune installs the open-source Equalizer APO engine (12 MB, hash-verified) and enables it for this output" };
                        ui.label(dim(format!("{what}, so the EQ runs in every app, even with RoomTune closed. One Windows permission prompt.")));
                    }
                    ui.label(dim("ASIO and exclusive-mode streams bypass the engine.").color(FAINT));
                });

                ui.add_space(6.0);
                if self.job == Some(Job::Setup) {
                    ui.add(egui::ProgressBar::new(self.progress.unwrap_or(0.0)).desired_width(ui.available_width()).text(mono(self.status.clone(), 11.0)));
                } else {
                    let label = if self.engine.ready() { "Continue" } else if self.engine.installed { "Enable engine & continue" } else { "Install engine & continue" };
                    let width = ui.available_width();
                    if ui.add_enabled_ui(idle && self.output().is_some(), |ui| accent_button(ui, label, vec2(width, 38.0))).inner.clicked() {
                        if self.engine.ready() { self.finish_setup(); } else { self.continue_after_setup = true; self.start_setup(); }
                    }
                    if !self.engine.ready() && ui.add_enabled(idle, egui::Link::new(dim("Skip for now"))).clicked() { self.finish_setup(); }
                    if self.status_error { ui.label(RichText::new(&self.status).size(12.0).color(LED_RED)); }
                }
            }));
        });
    }

    fn main_screen(&mut self, ctx: &egui::Context) {
        let idle = !self.busy();
        egui::TopBottomPanel::top("bar").exact_height(46.0).show_separator_line(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 14, right: 12, top: 8, bottom: 6 })).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                // Status display.
                let (color, on, text) = self.status_line();
                let width = (ui.available_width() - 150.0).clamp(200.0, 560.0);
                let (rect, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
                well(ui.painter(), rect);
                led(ui.painter(), pos2(rect.left() + 14.0, rect.center().y), color, on);
                let galley = ui.painter().layout(text, FontId::monospace(11.5), TEXT, rect.width() - 36.0);
                ui.painter().galley(pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0), galley, TEXT);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.menu(ui, idle);
                    ui.add_space(6.0);
                    let can_toggle = idle && self.engine.ready() && !self.demo() && !self.restart_needed;
                    let active = self.engine.active();
                    let clicked = ui.add_enabled_ui(can_toggle, |ui| hw_button(ui, "EQ", vec2(64.0, 28.0), Some(LED_GREEN), active)).inner.clicked();
                    if clicked { if active { self.bypass(); } else { self.apply(); } }
                });
            });
        });

        egui::TopBottomPanel::bottom("status").exact_height(32.0).show_separator_line(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 14, right: 14, top: 4, bottom: 6 })).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                let text = if self.status.is_empty() { "Ready".to_string() } else { self.status.clone() };
                ui.label(mono(text, 10.5).color(if self.status_error { LED_RED } else { DIM }));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut parts = vec![];
                    if self.eq.room.is_some() || !self.eq.manual.left.is_empty() { parts.push(format!("PREAMP {:+.1} dB", self.eq.preamp())); }
                    if let Some(d) = self.output() { parts.push(format!("{:.1} kHz", d.sample_rate as f32 / 1000.0)); parts.push(format!("{} CH", d.channels)); }
                    ui.label(mono(parts.join("  |  "), 10.5).color(FAINT));
                });
            });
        });

        egui::TopBottomPanel::bottom("rack").show_separator_line(false)
            .frame(egui::Frame::new().fill(PANEL).stroke(Stroke::new(1.0, EDGE)).inner_margin(egui::Margin { left: 14, right: 14, top: 10, bottom: 10 })).show(ctx, |ui| {
            let before = (self.eq.manual.clone(), self.eq.room_enabled);
            ui.horizontal(|ui| {
                self.room_section(ui, idle);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                self.band_section(ui);
            });
            if (self.eq.manual.clone(), self.eq.room_enabled) != before { self.eq.manual.sync(); self.mark_dirty(); }
        });

        egui::SidePanel::right("meter").exact_width(66.0).resizable(false).show_separator_line(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 4, right: 12, top: 6, bottom: 8 })).show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let painter = ui.painter();
                painter.text(pos2(rect.center().x, rect.top()), Align2::CENTER_TOP, "OUT", FontId::new(10.0, FontFamily::Name("semi".into())), DIM);
                let peak = self.out_meter.as_ref().map_or(FLOOR_DB, |m| m.hold[0].max(m.hold[1]));
                let readout = Rect::from_min_size(pos2(rect.left(), rect.top() + 16.0), vec2(rect.width(), 18.0));
                well(painter, readout);
                painter.text(readout.center(), Align2::CENTER_CENTER, if peak > FLOOR_DB + 1.0 { format!("{peak:+.1}") } else { "-inf".into() },
                    FontId::monospace(10.5), if peak > -3.0 { LED_RED } else { TEXT });
                let clip = peak > -0.3;
                let bars = Rect::from_min_max(pos2(rect.left(), readout.bottom() + 14.0), pos2(rect.left() + 26.0, rect.bottom() - 16.0));
                led(painter, pos2(bars.center().x, readout.bottom() + 7.0), LED_RED, clip);
                for slot in 0..2 {
                    let r = Rect::from_min_size(bars.min + vec2(slot as f32 * 14.0, 0.0), vec2(12.0, bars.height()));
                    let (level, hold) = self.out_meter.as_ref().map_or((FLOOR_DB, FLOOR_DB), |m| (m.level[slot], m.hold[slot]));
                    led_meter(painter, r, level, hold, true);
                    painter.text(pos2(r.center().x, rect.bottom()), Align2::CENTER_BOTTOM, ["L", "R"][slot], FontId::monospace(9.5), DIM);
                }
                for db in [0, -6, -12, -24, -36, -48] {
                    let y = bars.bottom() - bars.height() * (1.0 - db as f32 / FLOOR_DB);
                    painter.line_segment([pos2(bars.right() + 2.0, y), pos2(bars.right() + 5.0, y)], Stroke::new(1.0, FAINT));
                    painter.text(pos2(rect.right(), y), Align2::RIGHT_CENTER, db.to_string(), FontId::monospace(9.0), FAINT);
                }
            });

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 12, right: 4, top: 2, bottom: 8 })).show(ctx, |ui| {
            let editable = !self.demo() && !self.calibrating();
            let before = self.eq.manual.clone();
            let selected = editable.then_some(&mut self.selected);
            plot(ui, &mut self.eq, &mut self.plot_view, selected);
            if self.eq.manual != before { self.mark_dirty(); }
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.add_enabled_ui(idle, |ui| ui.menu_button(semi("MENU", 11.0), |ui| {
            ui.set_min_width(210.0);
            if ui.button("Devices & engine…").clicked() { self.screen = Screen::Setup; self.refresh_devices(); ui.close(); }
            ui.separator();
            if ui.add_enabled(self.output().is_some(), egui::Button::new("Import SoundID measurement…")).clicked() { ui.close(); self.import_soundid(); }
            ui.separator();
            if ui.button("Open EQ…").clicked() { ui.close(); self.open(); }
            if ui.add_enabled(!self.demo(), egui::Button::new("Save EQ…")).clicked() { ui.close(); self.save(false); }
            if ui.add_enabled(!self.demo() && self.target_id().is_some(), egui::Button::new("Export for Equalizer APO…")).clicked() { ui.close(); self.save(true); }
            ui.separator();
            if ui.button(if self.demo() { "Exit demo" } else { "Show demo" }).clicked() { self.toggle_demo(); ui.close(); }
        }));
    }

    fn room_section(&mut self, ui: &mut egui::Ui, idle: bool) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(caps("Room correction"));
            ui.horizontal(|ui| {
                if let Some(room) = &self.eq.room {
                    let (l, r) = (room.left.filters.len(), room.right.filters.len());
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
                    if ui.add_enabled_ui(!self.demo(), |ui| hw_button(ui, "ROOM", vec2(72.0, 26.0), Some(LED_GREEN), enabled)).inner
                        .on_hover_text(tip).clicked() { self.eq.room_enabled = !enabled; }
                    let (rect, _) = ui.allocate_exact_size(vec2(84.0, 26.0), Sense::hover());
                    well(ui.painter(), rect);
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, format!("L{l} R{r} FILT"), FontId::monospace(11.0), if enabled { TEXT } else { FAINT });
                } else {
                    let (rect, _) = ui.allocate_exact_size(vec2(160.0, 26.0), Sense::hover());
                    well(ui.painter(), rect);
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, "NOT MEASURED", FontId::monospace(11.0), FAINT);
                }
                let can = idle && self.input().is_some() && self.output().is_some();
                if ui.add_enabled_ui(can, |ui| hw_button(ui, "MEASURE…", vec2(84.0, 26.0), None, false)).inner.clicked() {
                    self.calibrating_open = true; self.ready = false;
                }
            });
        });
    }

    fn band_section(&mut self, ui: &mut egui::Ui) {
        let editable = !self.demo() && !self.calibrating();
        ui.add_enabled_ui(editable, |ui| ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(caps("Bands"));
            ui.horizontal(|ui| {
                let bands = self.eq.manual.bands_mut(false);
                if self.selected.is_some_and(|i| i >= bands.len()) { self.selected = None; }
                for (i, b) in bands.iter().enumerate() {
                    let (rect, response) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
                    let selected = self.selected == Some(i);
                    let color = band_color(i);
                    ui.painter().rect_filled(rect, 2.0, if selected { color.gamma_multiply(0.35) } else if response.hovered() { Color32::from_rgb(44, 48, 54) } else { Color32::from_rgb(36, 40, 45) });
                    ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, if selected { color } else { Color32::from_rgb(10, 11, 13) }), StrokeKind::Inside);
                    ui.painter().rect_filled(Rect::from_min_size(rect.left_top() + vec2(3.0, 2.0), vec2(rect.width() - 6.0, 2.0)), 0.0, if b.enabled { color } else { FAINT });
                    ui.painter().text(rect.center() + vec2(0.0, 2.0), Align2::CENTER_CENTER, (i + 1).to_string(), FontId::monospace(12.0), if b.enabled { TEXT } else { FAINT });
                    if response.on_hover_text(format!("{} · {} Hz · {:+.1} dB · Q {:.2}", b.kind.label(), short_hz(b.frequency), b.gain, b.q)).clicked() {
                        self.selected = if selected { None } else { Some(i) };
                    }
                }
                if bands.len() < eq::MAX_BANDS {
                    ui.menu_button(mono("+", 14.0), |ui| {
                        for (kind, f, q) in [(Kind::Peak, 1000.0, 1.0), (Kind::LowShelf, 120.0, 0.71), (Kind::HighShelf, 6000.0, 0.71)] {
                            if ui.button(kind.label()).clicked() {
                                bands.push(Band { kind, ..Band::peak(f, 0.0, q) });
                                self.selected = Some(bands.len() - 1);
                                ui.close();
                            }
                        }
                    });
                }
            });
        }));
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        ui.add_enabled_ui(editable, |ui| {
            let bands = self.eq.manual.bands_mut(false);
            match self.selected.and_then(|i| bands.get_mut(i).map(|b| (i, b))) {
                Some((i, b)) => {
                    let (rect, _) = ui.allocate_exact_size(vec2(4.0, 40.0), Sense::hover());
                    ui.painter().rect_filled(rect, 1.0, band_color(i));
                    param(ui, &format!("Band {}", i + 1), |ui| {
                        egui::ComboBox::from_id_salt("kind").width(92.0).selected_text(b.kind.label()).show_ui(ui, |ui| {
                            for k in Kind::ALL { ui.selectable_value(&mut b.kind, k, k.label()); }
                        });
                    });
                    let speed = b.frequency * 0.004;
                    param(ui, "Freq", |ui| ui.add_sized(vec2(82.0, 22.0), egui::DragValue::new(&mut b.frequency).range(eq::FREQ_RANGE).speed(speed).fixed_decimals(0).suffix(" Hz")));
                    param(ui, "Gain", |ui| ui.add_sized(vec2(78.0, 22.0), egui::DragValue::new(&mut b.gain).range(eq::GAIN_RANGE).speed(0.05).fixed_decimals(1).suffix(" dB")));
                    param(ui, "Q", |ui| ui.add_sized(vec2(56.0, 22.0), egui::DragValue::new(&mut b.q).range(eq::Q_RANGE).speed(0.01).fixed_decimals(2)));
                    let mut remove = false;
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(caps(" "));
                        ui.horizontal(|ui| {
                            if hw_button(ui, "ON", vec2(52.0, 22.0), Some(LED_GREEN), b.enabled).clicked() { b.enabled = !b.enabled; }
                            let delete = ui.input(|inp| inp.key_pressed(egui::Key::Delete)) && !ui.ctx().wants_keyboard_input();
                            remove = hw_button(ui, "DEL", vec2(44.0, 22.0), None, false).clicked() || delete;
                        });
                    });
                    if remove { bands.remove(i); self.selected = None; }
                }
                None => {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(caps("Edit"));
                        ui.label(dim(if bands.is_empty() { "Double-click the graph to add a band" } else { "Select a band · drag nodes on the graph · scroll a node for Q" }));
                    });
                }
            }
        });
    }

    fn calibrate_dialog(&mut self, ctx: &egui::Context) {
        let running = self.calibrating();
        let modal = egui::Modal::new(egui::Id::new("calibrate")).frame(egui::Frame::new().fill(PANEL).stroke(Stroke::new(1.0, EDGE)).corner_radius(4.0).inner_margin(18))
            .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(semi("ROOM MEASUREMENT", 15.0));
            ui.label(dim("A 10 s sweep per speaker at each mic position. The positions are averaged, then corrected toward flat across the speakers' usable range."));
            ui.add_space(12.0);
            let in_session = !self.session.is_empty();
            ui.add_enabled_ui(!running && !in_session, |ui| {
                ui.label(caps("Positions"));
                ui.horizontal(|ui| {
                    for n in [1usize, 3, 5, 7] {
                        if hw_button(ui, &n.to_string(), vec2(40.0, 24.0), Some(AMBER), self.settings.positions == n).clicked() {
                            self.settings.positions = n; self.save_settings();
                        }
                    }
                    ui.label(dim(format!("≈ {} s · more positions = more reliable", self.settings.positions * 25)).color(FAINT));
                });
                ui.add_space(6.0);
            });
            ui.add_enabled_ui(!running && !in_session, |ui| {
                ui.label(caps("Microphone"));
                ui.horizontal(|ui| {
                    let channels = self.input().map_or(1, |d| d.channels as usize);
                    let width = ui.available_width();
                    ui.allocate_ui(vec2(width - 102.0, 24.0), |ui| {
                        if device_combo(ui, "cal-input", &mut self.settings.input_id, &self.devices.inputs, "No microphone found") {
                            self.settings.input_channel = 0; self.save_settings();
                        }
                    });
                    if channel_combo(ui, "cal-channel", &mut self.settings.input_channel, channels) { self.save_settings(); }
                });
                ui.add_space(6.0);
                horizontal_meter(ui, self.in_meter.as_ref(), false, "Clap to check the input");
                ui.add_space(8.0);
                ui.label(caps("Sweep level"));
                ui.spacing_mut().slider_width = ui.available_width() - 96.0;
                if ui.add(egui::Slider::new(&mut self.settings.level_db, -42.0..=-18.0).step_by(3.0).fixed_decimals(0).suffix(" dBFS")).changed() { self.save_settings(); }
                ui.label(dim("Start with moderate speaker volume.").color(FAINT));
                ui.add_space(6.0);
                ui.label(caps("Mic calibration"));
                ui.horizontal(|ui| {
                    ui.label(mono(&self.mic.name, 11.5));
                    if ui.link(if self.mic.name == MicCalibration::default().name { "Load…" } else { "Change…" }).clicked() { self.load_mic(); }
                });
                ui.add_space(8.0);
                ui.checkbox(&mut self.ready, "Room quiet · moderate volume · other EQ and mic processing off");
            });
            ui.add_space(10.0);

            // Where the mic goes next.
            let total = self.settings.positions.clamp(1, POSITIONS.len());
            let current = self.session.len().min(total - 1);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::hover());
                well(ui.painter(), rect);
                let center = rect.center() + vec2(0.0, 8.0);
                ui.painter().text(pos2(rect.center().x, rect.top() + 4.0), Align2::CENTER_TOP, "SPEAKERS", FontId::monospace(8.5), FAINT);
                for (i, (_, offset)) in POSITIONS.iter().take(total).enumerate() {
                    let p = center + *offset * 26.0;
                    let (color, on) = if i < self.session.len() { (LED_GREEN, true) } else if i == current { (AMBER, true) } else { (FAINT, false) };
                    led(ui.painter(), p, color, on);
                }
                ui.vertical(|ui| {
                    ui.label(caps(&format!("Position {} of {total}", current + 1)));
                    ui.label(RichText::new(POSITIONS[current].0).color(TEXT).size(14.0));
                    ui.label(dim("Mic pointing at the ceiling (or as its manual says), at ear height unless stated."));
                });
            });
            ui.add_space(12.0);
            if running {
                ui.add(egui::ProgressBar::new(self.progress.unwrap_or(0.0)).desired_width(ui.available_width()).text(mono(self.status.clone(), 11.0)));
                ui.add_space(8.0);
                if hw_button(ui, "CANCEL", vec2(90.0, 28.0), None, false).clicked() { self.cancel.store(true, Ordering::Relaxed); }
            } else {
                ui.horizontal(|ui| {
                    let can = self.ready && self.input().is_some();
                    let label = format!("Measure position {}", current + 1);
                    if ui.add_enabled_ui(can, |ui| accent_button(ui, &label, vec2(200.0, 30.0))).inner.clicked() { self.calibrate(); }
                    if in_session && hw_button(ui, &format!("USE {} NOW", self.session.len()), vec2(96.0, 30.0), None, false).clicked() { self.finish_session(); }
                    if hw_button(ui, if in_session { "DISCARD" } else { "CLOSE" }, vec2(80.0, 30.0), None, false).clicked() {
                        self.abandon_session(); self.calibrating_open = false;
                    }
                });
            }
        });
        if modal.should_close() && !running { self.abandon_session(); self.calibrating_open = false; }
    }

    fn error_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.error.clone() else { return; };
        let modal = egui::Modal::new(egui::Id::new("error")).frame(egui::Frame::new().fill(PANEL).stroke(Stroke::new(1.0, EDGE)).corner_radius(4.0).inner_margin(18))
            .show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                led(ui.painter(), rect.center(), LED_RED, true);
                ui.label(semi("ATTENTION", 14.0));
            });
            ui.add_space(6.0);
            ui.label(RichText::new(message).color(TEXT));
            ui.add_space(12.0);
            hw_button(ui, "OK", vec2(72.0, 28.0), None, false).clicked()
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

/// A trace with a soft glow underneath, like an analyzer display.
fn glow_curve(painter: &egui::Painter, points: Vec<Pos2>, color: Color32, width: f32, dashed: bool) {
    if !dashed { painter.add(egui::Shape::line(points.clone(), Stroke::new(width + 5.0, color.gamma_multiply(0.12)))); }
    draw_curve(painter, points, Stroke::new(width, color), dashed);
}

/// Fill between a curve and the 0 dB line, fading toward the line.
fn fill_to_zero(painter: &egui::Painter, points: &[Pos2], base: f32, color: Color32) {
    let mut mesh = egui::Mesh::default();
    for w in points.windows(2) {
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(w[0], color);
        mesh.colored_vertex(pos2(w[0].x, base), Color32::TRANSPARENT);
        mesh.colored_vertex(w[1], color);
        mesh.colored_vertex(pos2(w[1].x, base), Color32::TRANSPARENT);
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
}

const MEASURED_SERIES: usize = 0;
const RESULT_SERIES: usize = 1;
const EQ_SERIES: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Legend { Series(usize), Speaker(usize) }

/// Which graph traces are shown (toggled from the legend).
struct PlotView { series: [bool; 3], speakers: [bool; 2] }

impl Default for PlotView { fn default() -> Self { Self { series: [true; 3], speakers: [true; 2] } } }

impl PlotView {
    fn shown(&self, item: Legend) -> bool { match item { Legend::Series(s) => self.series[s], Legend::Speaker(k) => self.speakers[k] } }

    fn toggle(&mut self, item: Legend) {
        let flag = match item { Legend::Series(s) => &mut self.series[s], Legend::Speaker(k) => &mut self.speakers[k] };
        *flag = !*flag;
        // Never hide both speakers.
        if !self.speakers.iter().any(|s| *s) { self.speakers = [true; 2]; }
    }
}

/// The analyzer display for both speakers: left solid, right dashed. With `selected`, the shared
/// manual bands are draggable nodes.
fn plot(ui: &mut egui::Ui, eq: &mut EqConfig, view: &mut PlotView, selected: Option<&mut Option<usize>>) {
    let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
    let outer = response.rect;
    let rect = Rect::from_min_max(outer.min + vec2(38.0, 30.0), outer.max - vec2(14.0, 24.0));
    let measured = eq.room.is_some();
    let max_gain = eq.manual.bands(false).iter().map(|b| b.gain.abs()).fold(0.0, f64::max);
    let range = if measured || max_gain > 10.5 { 18.0 } else { 12.0 };
    let x = |f: f64| rect.left() + ((f / 20.0).ln() / 1000_f64.ln()) as f32 * rect.width();
    let y = |db: f64| rect.top() + ((range - db.clamp(-range, range)) / (2.0 * range)) as f32 * rect.height();
    let freq_at = |px: f32| 20.0 * 1000_f64.powf(((px - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64);
    let db_at = |py: f32| range - ((py - rect.top()) / rect.height()) as f64 * 2.0 * range;
    let small = FontId::monospace(10.0);
    let clip = painter.with_clip_rect(rect);

    // Grid: every 1-9 × decade, majors at decades; 3 dB minor, labelled 6 dB steps.
    // The span the room correction was fitted over (union of both speakers).
    let ranges: Vec<(f64, f64)> = [false, true].into_iter().filter_map(|right| eq.measurement(right).and_then(|m| m.range)).collect();
    if !ranges.is_empty() {
        let (lo, hi) = ranges.iter().fold((f64::MAX, 0.0_f64), |(a, b), r| (a.min(r.0), b.max(r.1)));
        clip.rect_filled(Rect::from_x_y_ranges(x(lo)..=x(hi), rect.y_range()), 0.0, Color32::from_white_alpha(5));
    }
    for decade in [10.0, 100.0, 1000.0, 10000.0] {
        for k in 1..10 {
            let f = decade * k as f64;
            if !(20.0..=20000.0).contains(&f) { continue; }
            let major = k == 1;
            clip.line_segment([pos2(x(f), rect.top()), pos2(x(f), rect.bottom())], Stroke::new(1.0, if major { GRID_MAJOR } else { GRID }));
        }
    }
    for f in [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000] {
        painter.text(pos2(x(f as f64), rect.bottom() + 5.0), Align2::CENTER_TOP, short_hz(f as f64), small.clone(), DIM);
    }
    for db in (-(range as i32)..=range as i32).step_by(3) {
        let labelled = db % 6 == 0;
        let stroke = if db == 0 { Stroke::new(1.0, Color32::from_rgb(64, 70, 78)) } else if labelled { Stroke::new(1.0, GRID_MAJOR) } else { Stroke::new(1.0, GRID) };
        clip.line_segment([pos2(rect.left(), y(db as f64)), pos2(rect.right(), y(db as f64))], stroke);
        if labelled { painter.text(pos2(rect.left() - 6.0, y(db as f64)), Align2::RIGHT_CENTER, format!("{db:+}").replace("+0", "0"), small.clone(), DIM); }
    }
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, GRID_MAJOR), StrokeKind::Outside);

    // Hatched spans where either speaker's sweep was too close to the noise floor to trust.
    let noisy: Vec<(f64, f64)> = [false, true].into_iter().filter_map(|right| eq.measurement(right))
        .flat_map(|m| noisy_ranges(&m.frequencies, &m.snr)).collect();
    for &(lo, hi) in &noisy {
        let span = Rect::from_x_y_ranges(x(lo / 1.015)..=x(hi * 1.015), rect.y_range());
        let hatch = clip.with_clip_rect(span.intersect(rect));
        hatch.rect_filled(span, 0.0, Color32::from_rgba_unmultiplied(240, 204, 64, 10));
        let mut offset = -rect.height();
        while offset < span.width() {
            hatch.line_segment([pos2(span.left() + offset, rect.bottom()), pos2(span.left() + offset + rect.height(), rect.top())],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(240, 204, 64, 22)));
            offset += 7.0;
        }
    }

    // Interactive legend: hover an entry to highlight it, click to show or hide it.
    let mut legend_x = outer.left() + 14.0;
    let mut hovered: Option<Legend> = None;
    let mut items: Vec<(Legend, Color32, &str, bool, &str)> = vec![];
    if measured {
        items.push((Legend::Series(MEASURED_SERIES), MEASURED, "Measured", false, "The room as measured, before any EQ (smoothed)."));
        items.push((Legend::Series(RESULT_SERIES), CYAN, "Result", false, "Predicted response with the EQ applied: what you hear."));
    }
    items.push((Legend::Series(EQ_SERIES), AMBER, "EQ", false, "The correction itself: room correction plus your manual bands."));
    if measured {
        items.push((Legend::Speaker(0), TEXT, "Left", false, "Left speaker: solid lines."));
        items.push((Legend::Speaker(1), TEXT, "Right", true, "Right speaker: dashed lines."));
    }
    for (index, (item, color, text, dashed, tip)) in items.into_iter().enumerate() {
        let on = view.shown(item);
        let galley = painter.layout_no_wrap(text.to_uppercase(), FontId::new(10.0, FontFamily::Name("semi".into())), if on { TEXT } else { FAINT });
        let hit = Rect::from_min_size(pos2(legend_x - 4.0, outer.top() + 6.0), vec2(galley.size().x + 30.0, 17.0));
        let response = ui.interact(hit, response.id.with(("legend", index)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(format!("{tip}\nClick to {}.", if on { "hide" } else { "show" }));
        if response.hovered() { hovered = Some(item); painter.rect_filled(hit, 2.0, Color32::from_white_alpha(10)); }
        if response.clicked() { view.toggle(item); }
        let p = pos2(legend_x, outer.top() + 14.5);
        let swatch = if on { color } else { FAINT };
        draw_curve(&painter, vec![p, p + vec2(16.0, 0.0)], Stroke::new(if item == Legend::Series(RESULT_SERIES) { 2.6 } else { 1.8 }, swatch), dashed);
        let width = galley.size().x;
        painter.galley(p + vec2(21.0, -galley.size().y / 2.0), galley, TEXT);
        legend_x += 36.0 + width;
    }
    // Emphasis per series and speaker: hovered legend entry pops, everything else steps back.
    let alpha = |series: usize, right: bool| -> f32 {
        match hovered {
            Some(Legend::Series(s)) if s == series => 1.0,
            Some(Legend::Speaker(k)) if k == right as usize => 1.0,
            Some(_) => 0.18,
            None => 1.0,
        }
    };
    let visible = |series: usize, right: bool| view.series[series] && view.speakers[right as usize];
    if !noisy.is_empty() {
        let p = pos2(legend_x, outer.top() + 14.0);
        painter.rect_filled(Rect::from_center_size(p + vec2(7.0, 0.0), vec2(14.0, 9.0)), 1.0, Color32::from_rgba_unmultiplied(240, 204, 64, 60));
        painter.text(p + vec2(19.0, 0.0), Align2::LEFT_CENTER, "LOW SNR", FontId::new(9.5, FontFamily::Name("semi".into())), DIM);
    }
    if eq.is_demo() { painter.text(pos2(rect.right() - 8.0, rect.top() + 8.0), Align2::RIGHT_TOP, "DEMO DATA", small.clone(), LED_YELLOW); }

    // Traces.
    let freqs = log_grid(20.0, 20_000.0, 2_f64.powf(1.0 / 24.0));
    let base = y(0.0);
    // Draw order = importance: EQ fill, measured (background), EQ curves, result (on top).
    if view.series[EQ_SERIES] {
        let manual: Vec<Pos2> = freqs.iter().map(|&f| pos2(x(f), y(eq.manual.bands(false).iter().map(|b| b.db(f)).sum()))).collect();
        fill_to_zero(&clip, &manual, base, Color32::from_rgba_unmultiplied(255, 166, 64, (40.0 * alpha(EQ_SERIES, false)) as u8));
    }
    if let Some(i) = selected.as_ref().and_then(|s| **s) && let Some(b) = eq.manual.bands(false).get(i) {
        let own: Vec<Pos2> = freqs.iter().map(|&f| pos2(x(f), y(b.db(f)))).collect();
        let c = band_color(i);
        fill_to_zero(&clip, &own, base, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 70));
        draw_curve(&clip, own, Stroke::new(1.2, c.gamma_multiply(0.8)), false);
    }
    // Measured responses, smoothed as the fit sees them.
    let smoothed: [Option<Vec<f64>>; 2] = [eq.measurement(false).map(|m| m.smoothed()), eq.measurement(true).map(|m| m.smoothed())];
    let line = |right: bool, db: &dyn Fn(f64, usize) -> f64| -> Vec<Pos2> {
        eq.measurement(right).map_or(vec![], |m| m.frequencies.iter().enumerate().map(|(i, &f)| pos2(x(f), y(db(f, i)))).collect())
    };
    for right in [false, true] {
        if let Some(s) = &smoothed[right as usize] && visible(MEASURED_SERIES, right) {
            let a = alpha(MEASURED_SERIES, right);
            let emphasized = hovered.is_some() && a == 1.0;
            draw_curve(&clip, line(right, &|_, i| s[i]), Stroke::new(if emphasized { 2.0 } else { 1.2 }, MEASURED.gamma_multiply(if emphasized { 1.0 } else { 0.5 * a })), right);
        }
    }
    let room_differs = eq.room_enabled && eq.room.as_ref().is_some_and(|p| !p.left.filters.is_empty() || !p.right.filters.is_empty());
    for right in if room_differs { vec![false, true] } else { vec![false] } {
        if !visible(EQ_SERIES, right) && room_differs { continue; }
        if !view.series[EQ_SERIES] { continue; }
        let a = alpha(EQ_SERIES, right);
        let points = freqs.iter().map(|&f| pos2(x(f), y(eq.response(right, f)))).collect();
        draw_curve(&clip, points, Stroke::new(1.7, AMBER.gamma_multiply(0.85 * a)), right);
    }
    for right in [false, true] {
        if let Some(s) = &smoothed[right as usize] && visible(RESULT_SERIES, right) {
            let a = alpha(RESULT_SERIES, right);
            let points = line(right, &|f, i| s[i] + eq.response(right, f));
            // Outside the corrected range the result is just the speaker's roll-off: keep it quiet.
            let (lo, hi) = eq.measurement(right).and_then(|m| m.range).map_or((0.0, f32::MAX), |(lo, hi)| (x(lo), x(hi)));
            let (inside, outside): (Vec<Pos2>, Vec<Pos2>) = (points.iter().copied().filter(|p| (lo..=hi).contains(&p.x)).collect(),
                points.iter().copied().filter(|p| p.x <= lo + 1.0).collect());
            let above: Vec<Pos2> = points.iter().copied().filter(|p| p.x >= hi - 1.0).collect();
            for part in [outside, above] { draw_curve(&clip, part, Stroke::new(1.3, CYAN.gamma_multiply(0.35 * a)), right); }
            if a == 1.0 { glow_curve(&clip, inside, CYAN, 2.4, right); } else { draw_curve(&clip, inside, Stroke::new(1.6, CYAN.gamma_multiply(a)), right); }
        }
    }

    // Band nodes.
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
                readout = Some(format!("BAND {}  {}  {:>7} Hz  {:+5.1} dB  Q {:.2}", i + 1, b.kind.label().to_uppercase(), short_hz(b.frequency), b.gain, b.q));
            }
            let is_selected = *selected == Some(i);
            let color = if b.enabled { band_color(i) } else { FAINT };
            if is_selected { painter.circle_filled(center, 13.0, color.gamma_multiply(0.18)); }
            painter.circle(center, if is_selected { 8.5 } else { 7.0 }, color, Stroke::new(if is_selected { 2.0 } else { 1.5 }, if is_selected { Color32::WHITE } else { WELL }));
            painter.text(center, Align2::CENTER_CENTER, (i + 1).to_string(), FontId::monospace(9.5), WELL);
        }
        if response.double_clicked() && bands.len() < eq::MAX_BANDS && let Some(p) = response.interact_pointer_pos().filter(|p| rect.contains(*p)) {
            bands.push(Band::peak(round_frequency(freq_at(p.x)), (db_at(p.y).clamp(-12.0, 12.0) * 10.0).round() / 10.0, 1.0));
            *selected = Some(bands.len() - 1);
            changed = true;
        }
        if response.clicked() && !changed { *selected = None; }
        if changed { eq.manual.sync(); }
    }

    // Crosshair readout.
    if readout.is_none() && !dragging && let Some(pointer) = response.hover_pos().filter(|p| rect.contains(*p)) {
        let f = freq_at(pointer.x);
        clip.line_segment([pos2(pointer.x, rect.top()), pos2(pointer.x, rect.bottom())], Stroke::new(1.0, Color32::from_white_alpha(28)));
        clip.line_segment([pos2(rect.left(), pointer.y), pos2(rect.right(), pointer.y)], Stroke::new(1.0, Color32::from_white_alpha(14)));
        let (l, r) = (eq.response(false, f), eq.response(true, f));
        let mut text = if (l - r).abs() < 0.05 { format!("{:>6} Hz   EQ {l:+5.1}", short_hz(f)) } else { format!("{:>6} Hz   EQ L {l:+5.1} R {r:+5.1}", short_hz(f)) };
        let with_eq = |right: bool| eq.measurement(right).zip(smoothed[right as usize].as_ref()).map(|(m, s)| {
            let i = (0..m.frequencies.len()).min_by(|&a, &b| (m.frequencies[a] / f).ln().abs().total_cmp(&(m.frequencies[b] / f).ln().abs())).unwrap_or(0);
            s[i] + eq.response(right, m.frequencies[i])
        });
        if let (Some(l), Some(r)) = (with_eq(false), with_eq(true)) { text += &format!("   OUT L {l:+5.1} R {r:+5.1} dB"); }
        readout = Some(text);
    }
    if let Some(text) = readout {
        let galley = painter.layout_no_wrap(text, FontId::monospace(10.5), TEXT);
        let pos = pos2(outer.right() - 14.0 - galley.size().x, outer.top() + 14.0 - galley.size().y / 2.0);
        let frame = Rect::from_min_size(pos, galley.size()).expand2(vec2(7.0, 3.0));
        painter.rect_filled(frame, 2.0, Color32::from_rgb(22, 25, 28));
        painter.rect_stroke(frame, 2.0, Stroke::new(1.0, EDGE), StrokeKind::Inside);
        painter.galley(pos, galley, TEXT);
    }
}

fn style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut semi_family = fallback.clone();
    // Native Windows fonts when present (Segoe UI, Cascadia Mono / Consolas); egui's bundled fonts are the fallback.
    if let Some(windir) = std::env::var_os("WINDIR").map(PathBuf::from) {
        let dir = windir.join("Fonts");
        let mut load = |name: &str, files: &[&str]| files.iter().find_map(|f| std::fs::read(dir.join(f)).ok())
            .map(|bytes| { fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes))); name.to_string() });
        let segoe = load("segoe", &["segoeui.ttf"]);
        let semibold = load("segoe-semibold", &["seguisb.ttf"]);
        let mono = load("mono", &["CascadiaMono.ttf", "consola.ttf"]);
        if let Some(n) = segoe { fonts.families.entry(FontFamily::Proportional).or_default().insert(0, n.clone()); semi_family.insert(0, n); }
        if let Some(n) = semibold { semi_family.insert(0, n); }
        if let Some(n) = mono { fonts.families.entry(FontFamily::Monospace).or_default().insert(0, n); }
    }
    fonts.families.insert(FontFamily::Name("semi".into()), semi_family);
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.5));
        style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
        style.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.5));
        style.spacing.item_spacing = vec2(6.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;
        let v = &mut style.visuals;
        v.override_text_color = None;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.window_corner_radius = 4.0.into();
        v.window_stroke = Stroke::new(1.0, EDGE);
        v.menu_corner_radius = 3.0.into();
        v.extreme_bg_color = WELL;
        v.faint_bg_color = PANEL;
        v.hyperlink_color = AMBER;
        v.widgets.noninteractive.fg_stroke.color = TEXT;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, EDGE);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] { w.corner_radius = 2.0.into(); }
        v.widgets.inactive.bg_fill = Color32::from_rgb(46, 51, 57);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 44, 49);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(12, 13, 15));
        v.widgets.inactive.fg_stroke.color = TEXT;
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(52, 57, 64);
        v.widgets.hovered.bg_fill = Color32::from_rgb(60, 66, 74);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(80, 88, 98));
        v.widgets.active.weak_bg_fill = Color32::from_rgb(30, 33, 37);
        v.widgets.open.weak_bg_fill = Color32::from_rgb(30, 33, 37);
        v.selection.bg_fill = Color32::from_rgb(120, 78, 30);
        v.selection.stroke = Stroke::new(1.0, AMBER);
        v.slider_trailing_fill = true;
    });
}
