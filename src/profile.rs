use crate::dsp::{Measurement, demo_measurement};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::{Path, PathBuf}, time::{SystemTime, UNIX_EPOCH}};

pub const PROFILE_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub version: u32,
    pub output_id: String,
    pub output_name: String,
    pub microphone_name: String,
    pub calibration_name: String,
    pub created_unix: u64,
    pub demo: bool,
    pub left: Measurement,
    pub right: Measurement,
}

impl Profile {
    pub fn validate(&self, apply: bool) -> Result<()> {
        ensure!(self.version == PROFILE_VERSION, "Unsupported profile version");
        ensure!(!apply || !self.demo, "Demo data cannot be applied. Run a microphone calibration first");
        ensure!([&self.output_id, &self.output_name, &self.microphone_name, &self.calibration_name].iter().all(|s| s.len() <= 4096), "Profile metadata is too long");
        endpoint_guid(&self.output_id)?;
        self.left.validate()?; self.right.validate()?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        ensure!(fs::metadata(path)?.len() <= 2_000_000, "Profile file is too large");
        let text = fs::read_to_string(path)?;
        let profile: Self = serde_json::from_str(&text).context("Could not read RoomTune JSON profile")?;
        profile.validate(false)?; Ok(profile)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate(false)?;
        atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    pub fn demo() -> Self {
        Self { version: PROFILE_VERSION, output_id: "{00000000-0000-0000-0000-000000000000}".into(),
            output_name: "Simulated stereo speakers".into(), microphone_name: "Synthetic response".into(),
            calibration_name: "Demo only".into(), created_unix: now(), demo: true,
            left: demo_measurement(65.0), right: demo_measurement(82.0) }
    }
}

pub fn endpoint_guid(id: &str) -> Result<String> {
    let start = id.rfind('{').context("Output endpoint has no GUID")?;
    let guid = uuid::Uuid::parse_str(&id[start..]).context("Invalid output endpoint GUID")?;
    Ok(format!("{{{guid}}}"))
}

pub fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("RoomTune")
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(content)?; file.as_file().sync_all()?;
    // A reader holding the target open (Equalizer APO reloads its config right after each
    // change) makes the replace fail with "access denied" for a moment; wait it out.
    for _ in 0..40 {
        match file.persist(path) {
            Ok(_) => return Ok(()),
            Err(e) if e.error.kind() == std::io::ErrorKind::PermissionDenied => {
                file = e.file;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => return Err(e.error).context("Could not replace file atomically"),
        }
    }
    file.persist(path).map_err(|e| e.error).context("Could not replace file atomically")?;
    Ok(())
}
