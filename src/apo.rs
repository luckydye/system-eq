//! Playback-engine configuration through Equalizer APO's documented configuration files.
//!
//! RoomTune owns `config\RoomTune\*` and one marked block in `config.txt`. Everything else in the
//! configuration is preserved. The engine installer grants users write access to the config
//! folder, so EQ changes are written directly and take effect live. Only if that fails does the
//! app re-launch itself elevated (`--helper ...`) and wait for the result.
use crate::{engine, eq::EqConfig, platform, profile::{atomic_write, data_dir, endpoint_guid}};
use anyhow::{Context, Result, bail, ensure};
use std::{fs, path::{Path, PathBuf}};

pub const BEGIN: &str = "# BEGIN ROOMTUNE MANAGED BLOCK";
pub const END: &str = "# END ROOMTUNE MANAGED BLOCK";
const ENABLED: &str = "# RoomTune correction enabled";
const BYPASSED: &str = "# RoomTune bypassed\r\n";
const INCLUDE: &str = "Include: RoomTune\\active.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation { Apply, Bypass, Restore }

impl Operation {
    pub fn name(self) -> &'static str {
        match self { Self::Apply => "apply", Self::Bypass => "bypass", Self::Restore => "restore" }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name { "apply" => Self::Apply, "bypass" => Self::Bypass, "restore" => Self::Restore, _ => bail!("Unknown playback operation") })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status { NotInstalled, NotConfigured, Bypassed, Enabled }

pub struct Paths { pub config: PathBuf, pub owned: PathBuf, pub active: PathBuf, pub last_enabled: PathBuf }

impl Paths {
    pub fn new(root: &Path) -> Self {
        let dir = root.join("config"); let owned = dir.join("RoomTune");
        Self { config: dir.join("config.txt"), active: owned.join("active.txt"), last_enabled: owned.join("last-enabled.txt"), owned }
    }
}

pub fn find_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = ["InstallPath", "InstallDir"].iter()
        .filter_map(|name| platform::registry_string(r"SOFTWARE\EqualizerAPO", name)).filter(|p| !p.is_empty()).map(PathBuf::from).collect();
    candidates.extend(["ProgramW6432", "ProgramFiles"].iter().filter_map(std::env::var_os).map(|p| PathBuf::from(p).join("EqualizerAPO")));
    candidates.into_iter().find(|p| p.join("config").is_dir())
}

pub fn status_at(root: &Path) -> Status {
    let p = Paths::new(root);
    let config = fs::read_to_string(&p.config).unwrap_or_default();
    match fs::read_to_string(&p.active) {
        Ok(active) if config.contains(INCLUDE) => if active.contains(ENABLED) { Status::Enabled } else { Status::Bypassed },
        _ => Status::NotConfigured,
    }
}

pub fn status() -> Status { find_dir().map_or(Status::NotInstalled, |root| status_at(&root)) }

pub fn export(eq: &EqConfig) -> Result<String> {
    eq.validate(true)?;
    let mut lines = vec![
        ENABLED.to_string(),
        "# Written by RoomTune: room correction plus manual EQ, per speaker.".into(),
        format!("Device: {}", endpoint_guid(&eq.output_id)?),
        "Stage: post-mix".into(),
        "Channel: all".into(),
        format!("Preamp: {:.1} dB", eq.preamp()),
    ];
    for (name, right) in [("L", false), ("R", true)] {
        lines.push(format!("Channel: {name}"));
        for (i, band) in eq.filters(right).iter().enumerate() { lines.push(format!("Filter {}: {}", i + 1, band.apo())); }
    }
    lines.extend(["Channel: all".into(), "Device: all".into(), String::new()]);
    Ok(lines.join("\r\n"))
}

/// Adds (or replaces) RoomTune's marked include block, keeping everything else unchanged.
pub fn integrate(existing: &str) -> Result<String> {
    let a = existing.find(BEGIN); let b = existing.find(END);
    let duplicated = |m: &str, at: usize| existing[at + m.len()..].contains(m);
    let mut rest = existing.to_string();
    match (a, b) {
        (None, None) => {}
        (Some(a), Some(b)) if b > a && !duplicated(BEGIN, a) && !duplicated(END, b) => { rest.replace_range(a..b + END.len(), ""); }
        _ => bail!("The existing RoomTune block in config.txt is incomplete or duplicated. Inspect config.txt before applying"),
    }
    let block = [BEGIN, "Device: all", "Channel: all", "Stage: post-mix", INCLUDE, "Device: all", "Channel: all", END].join("\r\n");
    let head = rest.trim_end();
    Ok(if head.is_empty() { format!("{block}\r\n") } else { format!("{head}\r\n\r\n{block}\r\n") })
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("{} is not UTF-8 text; RoomTune will not rewrite it. Re-save it as UTF-8", path.display()))
}

/// Performs a playback change directly in an Equalizer APO installation at `root`.
pub fn write_operation(root: &Path, op: Operation, eq: Option<&EqConfig>) -> Result<()> {
    let p = Paths::new(root);
    fs::create_dir_all(&p.owned).context("Could not create the RoomTune folder in Equalizer APO's config")?;
    match op {
        Operation::Apply => {
            let content = export(eq.context("No EQ was supplied")?)?;
            let existing = if p.config.exists() { read_text(&p.config)? } else { String::new() };
            let integrated = integrate(&existing)?;
            let previous = fs::read_to_string(&p.active).unwrap_or_else(|_| BYPASSED.into());
            atomic_write(&p.active, content.as_bytes())?;
            if integrated != existing {
                let backup = || -> Result<()> {
                    if p.config.exists() { fs::copy(&p.config, p.owned.join(format!("config-backup-{}.txt", now_millis())))?; }
                    atomic_write(&p.config, integrated.as_bytes())
                };
                if let Err(e) = backup() { let _ = atomic_write(&p.active, previous.as_bytes()); return Err(e); }
            }
            atomic_write(&p.last_enabled, content.as_bytes())?;
        }
        Operation::Bypass => {
            if let Ok(active) = fs::read_to_string(&p.active) && active.contains(ENABLED) {
                atomic_write(&p.last_enabled, active.as_bytes())?;
            }
            atomic_write(&p.active, BYPASSED.as_bytes())?;
        }
        Operation::Restore => {
            ensure!(p.last_enabled.is_file(), "No previous RoomTune correction exists");
            atomic_write(&p.active, &fs::read(&p.last_enabled)?)?;
        }
    }
    Ok(())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// Runs this executable elevated as `--helper <args> --report <file>` and returns its exit code
/// (0, or 2 for "succeeded, restart Windows").
pub fn run_helper(mut args: Vec<String>) -> Result<u32> {
    fs::create_dir_all(data_dir())?;
    let report = data_dir().join(format!("helper-{}-{}.txt", std::process::id(), now_millis()));
    args.insert(0, "--helper".into());
    args.extend(["--report".into(), report.to_string_lossy().into_owned()]);
    let code = platform::run_self_elevated(&args)?;
    let message = fs::read_to_string(&report).ok();
    let _ = fs::remove_file(&report);
    ensure!(code == 0 || code == 2, message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "The administrator helper did not complete the change".into()));
    Ok(code)
}

/// Performs a playback change: directly when allowed, otherwise via the elevated helper.
pub fn execute(op: Operation, eq: Option<&EqConfig>) -> Result<()> {
    let root = find_dir().context("The playback engine is not installed. Use Set up playback engine first")?;
    match write_operation(&root, op, eq) {
        Err(e) if platform::is_access_denied(&e) && !platform::is_elevated() => {
            let mut args = vec![op.name().to_string()];
            if let Some(eq) = eq {
                let path = data_dir().join("apply.roomtune");
                eq.save(&path)?;
                args.push(path.to_string_lossy().into_owned());
            }
            run_helper(args).map(drop)
        }
        other => other,
    }
}

/// Elevated helper entry: `--helper <apply|bypass|restore|setup> [args] [--report <file>]`.
/// `setup <installer|-> <endpoint-guid>` installs/enables the engine; exit code 2 means a
/// Windows restart is still needed for the audio engine to pick it up.
pub fn helper_main(args: &[String]) -> i32 {
    let mut report = None; let mut rest = vec![];
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--report" { report = it.next().map(PathBuf::from); } else { rest.push(arg.as_str()); }
    }
    let result = (|| -> Result<i32> {
        let command = rest.first().copied().unwrap_or_default();
        if command == "setup" {
            let installer = rest.get(1).filter(|p| **p != "-").map(Path::new);
            let guid = endpoint_guid(rest.get(2).context("No output endpoint was supplied")?)?;
            return Ok(if engine::setup(installer, &guid)? { 0 } else { 2 });
        }
        let op = Operation::parse(command)?;
        let root = find_dir().context("The playback engine is not installed")?;
        let eq = rest.get(1).map(|p| EqConfig::load(Path::new(p))).transpose()?;
        write_operation(&root, op, eq.as_ref())?;
        Ok(0)
    })();
    match result {
        Ok(code) => code,
        Err(e) => { if let Some(report) = report { let _ = fs::write(report, format!("{e:#}")); } 1 }
    }
}
