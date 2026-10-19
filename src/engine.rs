//! Built-in setup of the playback engine. RoomTune uses Equalizer APO (GPLv2) as the audio
//! processing object inside Windows' audio engine. RoomTune downloads a pinned release, verifies
//! it, installs it silently, and enables it on the chosen output, making the same registry
//! changes as Equalizer APO's Device Selector, so the user never needs Equalizer APO's own tools.
use crate::{apo, platform::{self, Key}, profile::data_dir};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::{Path, PathBuf}, time::{Duration, Instant}};

// Pinned Equalizer APO 1.4.2 x64 installer.
const HOST: &str = "downloads.sourceforge.net";
const PATH: &str = "/project/equalizerapo/1.4.2/EqualizerAPO-x64-1.4.2.exe";
const FILE: &str = "EqualizerAPO-x64-1.4.2.exe";
/// SHA-256 of the official installer, cross-checked with the Chocolatey package checksum.
const SHA256: &str = "7403be7427bbe1936a40dded082829b6e217fc4f5990fee5cba501f0ae055afa";
const MAX_BYTES: u64 = 30_000_000;

pub const PRE_MIX: &str = "{EACD2258-FCAC-4FF4-B36D-419E924A6D79}";
pub const POST_MIX: &str = "{EC1CC9CE-FAED-4822-828A-82A81A6F018F}";
const RENDER: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
const CHILD_APOS: &str = r"SOFTWARE\EqualizerAPO\Child APOs";
const FX_TITLE: &str = "{b725f130-47ef-101a-a5f1-02608c9eebac},10";
const COMBINED_DEVICE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},41";
const DISABLE_ENHANCEMENTS: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
const DEFAULT_PROCESSING_MODE: &str = "{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}";
const NO_KEY: &str = "!KEY";
const NO_VALUE: &str = "!VALUE";

/// FxProperties APO slots, `{d04e05a6-594b-4fb6-a80d-01af5eed7d1d},N`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot { Lfx = 1, Gfx = 2, Sfx = 5, Mfx = 6, Efx = 7 }
const SLOTS: [Slot; 5] = [Slot::Lfx, Slot::Gfx, Slot::Sfx, Slot::Mfx, Slot::Efx];
const MULTI_SLOTS: [u32; 3] = [13, 14, 15];

fn slot_name(n: u32) -> String { format!("{{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}},{n}") }
fn modes_name(slot: Slot) -> String { format!("{{d3993a3f-99c2-4402-b5ec-a92a0367664b}},{}", slot as u32) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { LfxGfx, SfxMfx, SfxEfx }

/// Install mode selection of Device Selector for Windows 8.1+.
pub fn choose_mode(present: impl Fn(u32) -> bool, combined_device: bool) -> Mode {
    let modern = [Slot::Sfx, Slot::Mfx, Slot::Efx].iter().map(|s| *s as u32).chain(MULTI_SLOTS).any(&present);
    if (present(Slot::Lfx as u32) || present(Slot::Gfx as u32)) && !modern { Mode::LfxGfx }
    else if combined_device { Mode::SfxMfx }
    else { Mode::SfxEfx }
}

/// Picks the driver's original (child) APOs that Equalizer APO chains to, by mode.
pub fn child_apos(original: impl Fn(Slot) -> String, mode: Mode) -> (String, String) {
    let missing = |s: Slot| { let v = original(s); v == NO_VALUE || v == NO_KEY };
    let pick = |primary: Slot, other: Slot, fallback: Slot| {
        let chosen = if missing(primary) && missing(other) { fallback } else { primary };
        if missing(chosen) { String::new() } else { original(chosen) }
    };
    match mode {
        Mode::LfxGfx => (pick(Slot::Lfx, Slot::Gfx, Slot::Sfx), pick(Slot::Gfx, Slot::Lfx, Slot::Mfx)),
        Mode::SfxMfx => (pick(Slot::Sfx, Slot::Mfx, Slot::Lfx), pick(Slot::Mfx, Slot::Sfx, Slot::Gfx)),
        Mode::SfxEfx => (pick(Slot::Sfx, Slot::Efx, Slot::Lfx), pick(Slot::Efx, Slot::Sfx, Slot::Gfx)),
    }
}

/// Equalizer APO is installed and its COM object is registered.
pub fn installed() -> bool {
    apo::find_dir().is_some() && Key::exists(&format!(r"SOFTWARE\Classes\CLSID\{POST_MIX}"))
}

fn is_ours(value: &str) -> bool { value.eq_ignore_ascii_case(PRE_MIX) || value.eq_ignore_ascii_case(POST_MIX) }

/// Whether Equalizer APO is registered on this render endpoint (`{guid}`).
pub fn enabled_on(endpoint_guid: &str) -> bool {
    Key::open(&format!(r"{RENDER}\{endpoint_guid}\FxProperties"), false)
        .is_ok_and(|fx| SLOTS.iter().any(|s| fx.string(&slot_name(*s as u32)).is_some_and(|v| is_ours(&v))))
}

fn open_writable(path: &str) -> Result<Key> {
    match Key::open(path, true) {
        Err(e) if platform::is_access_denied(&e) => { platform::make_key_writable(path)?; Key::open(path, true) }
        other => other,
    }
}

/// Registers Equalizer APO on a render endpoint. Needs elevation. Compatible with Device
/// Selector, so Equalizer APO's own uninstaller restores the driver's original effects.
pub fn enable(endpoint_guid: &str) -> Result<()> {
    let device = format!(r"{RENDER}\{endpoint_guid}");
    ensure!(Key::exists(&device), "This output is not registered as a Windows playback endpoint");
    if enabled_on(endpoint_guid) { return Ok(()); }
    let fx_path = format!(r"{device}\FxProperties");
    let existing = Key::open(&fx_path, false).ok();
    let original = |s: Slot| match &existing {
        None => NO_KEY.to_string(),
        Some(fx) => fx.string(&slot_name(s as u32)).unwrap_or_else(|| NO_VALUE.into()),
    };
    let present = |n: u32| existing.as_ref().is_some_and(|fx| fx.has(&slot_name(n)));
    let combined = Key::open(&format!(r"{device}\Properties"), false).is_ok_and(|p| p.has(COMBINED_DEVICE));
    let mode = choose_mode(present, combined);
    let (pre_child, post_child) = child_apos(original, mode);

    Key::create(CHILD_APOS)?;
    let child = Key::create(&format!(r"{CHILD_APOS}\{endpoint_guid}"))?;
    for s in SLOTS { child.set_string(&slot_name(s as u32), &original(s))?; }
    child.set_string("PreMixChild", &pre_child)?;
    child.set_string("PostMixChild", &post_child)?;
    child.set_string("AllowSilentBufferModification", "false")?;
    child.delete("DisableAutomaticAdjustment")?;
    child.set_string("Version", "2")?;
    drop(existing);

    let fx = if Key::exists(&fx_path) { open_writable(&fx_path)? } else {
        let created = Key::create(&fx_path).or_else(|e| {
            if !platform::is_access_denied(&e) { return Err(e); }
            platform::make_key_writable(&device)?;
            Key::create(&fx_path)
        })?;
        created.set_string(FX_TITLE, "Equalizer APO")?;
        created
    };
    let put = |slot: Slot, apo: &str| -> Result<()> {
        fx.set_string(&slot_name(slot as u32), apo)?;
        if slot != Slot::Lfx && slot != Slot::Gfx && !fx.has(&modes_name(slot)) { fx.set_multi(&modes_name(slot), DEFAULT_PROCESSING_MODE)?; }
        Ok(())
    };
    match mode {
        Mode::LfxGfx => {
            put(Slot::Lfx, PRE_MIX)?; put(Slot::Gfx, POST_MIX)?;
            for s in [Slot::Sfx, Slot::Mfx, Slot::Efx] { fx.delete(&slot_name(s as u32))?; }
        }
        Mode::SfxMfx | Mode::SfxEfx => {
            for s in [Slot::Lfx, Slot::Gfx] { fx.delete(&slot_name(s as u32))?; }
            put(Slot::Sfx, PRE_MIX)?;
            put(if mode == Mode::SfxMfx { Slot::Mfx } else { Slot::Efx }, POST_MIX)?;
        }
    }
    // Equalizer APO only runs when audio enhancements are allowed on the endpoint.
    fx.delete(DISABLE_ENHANCEMENTS)?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> String { Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() }

fn installer_path() -> PathBuf { data_dir().join("downloads").join(FILE) }

/// Downloads and verifies the installer, reusing a previously verified copy.
pub fn download(progress: &dyn Fn(f32), cancelled: &dyn Fn() -> bool) -> Result<PathBuf> {
    ensure!(cfg!(target_arch = "x86_64"), "Automatic engine setup currently supports 64-bit Intel/AMD Windows only");
    let path = installer_path();
    if fs::read(&path).is_ok_and(|b| sha256(&b) == SHA256) { return Ok(path); }
    fs::create_dir_all(path.parent().unwrap())?;
    let mut data = Vec::with_capacity(13_000_000);
    platform::download(HOST, PATH, &mut data, MAX_BYTES, progress, cancelled)?;
    ensure!(sha256(&data) == SHA256, "The downloaded engine installer failed verification and was discarded. Try again later");
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(&data)?;
    file.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

/// Elevated: re-verifies the installer from an admin-only folder and runs it silently.
/// The installer opens Device Selector at the end; RoomTune configures the device itself, so
/// that window is closed as soon as it appears.
pub fn install(installer: &Path) -> Result<()> {
    let bytes = fs::read(installer).context("Could not read the engine installer")?;
    ensure!(sha256(&bytes) == SHA256, "The engine installer was modified after download. Nothing was installed");
    let windir = std::env::var_os("SystemRoot").map(PathBuf::from).context("SystemRoot is not set")?;
    // Objects an elevated admin creates in %SystemRoot%\Temp are not writable by unelevated processes.
    let dir = tempfile::Builder::new().prefix("RoomTune-").tempdir_in(windir.join("Temp"))?;
    let exe = dir.path().join(FILE);
    fs::write(&exe, &bytes)?;
    let mut child = std::process::Command::new(&exe).arg("/S").spawn().context("Could not start the engine installer")?;
    let started = Instant::now();
    loop {
        platform::kill_processes("DeviceSelector.exe");
        if let Some(status) = child.try_wait()? {
            // Exit code 0 or the NSIS reboot-flag code are both success; RoomTune verifies below.
            let _ = status;
            break;
        }
        if started.elapsed() > Duration::from_secs(300) { let _ = child.kill(); bail!("The engine installer did not finish within 5 minutes"); }
        std::thread::sleep(Duration::from_millis(150));
    }
    platform::kill_processes("DeviceSelector.exe");
    ensure!(installed(), "The engine installer finished, but Equalizer APO is not registered. Restart Windows and try again");
    Ok(())
}

/// Elevated: restarts the Windows audio services so the endpoint loads its new effects.
pub fn restart_audio() -> bool {
    let stopped = platform::run_hidden("net", &["stop", "AudioEndpointBuilder", "/y"]);
    let started = platform::run_hidden("net", &["start", "Audiosrv"]);
    stopped && started
}

/// Elevated setup entry: optional install, then enable on the endpoint, then restart audio.
/// Returns whether the audio restart worked (otherwise a Windows restart is needed).
pub fn setup(installer: Option<&Path>, endpoint_guid: &str) -> Result<bool> {
    if let Some(installer) = installer.filter(|_| !installed()) { install(installer)?; }
    ensure!(installed(), "Equalizer APO is not installed");
    let was_enabled = enabled_on(endpoint_guid);
    enable(endpoint_guid)?;
    Ok(was_enabled || restart_audio())
}
