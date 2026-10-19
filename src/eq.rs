//! The complete playback EQ: optional room correction plus user-editable bands per speaker.
use crate::{dsp::{Measurement, log_grid}, profile::{Profile, atomic_write, endpoint_guid}};
use anyhow::{Context, Result, ensure};
use rustfft::num_complex::Complex64;
use serde::{Deserialize, Serialize};
use std::{f64::consts::PI, fs, path::Path};

pub const EQ_VERSION: u32 = 3;
pub const MAX_BANDS: usize = 16;
pub const GAIN_RANGE: std::ops::RangeInclusive<f64> = -24.0..=12.0;
pub const Q_RANGE: std::ops::RangeInclusive<f64> = 0.1..=10.0;
pub const FREQ_RANGE: std::ops::RangeInclusive<f64> = 20.0..=20_000.0;
/// Nominal rate for predicting manual bands; shapes are practically rate-independent below 20 kHz.
const RATE: f64 = 48_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind { Peak, LowShelf, HighShelf }

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Peak, Kind::LowShelf, Kind::HighShelf];
    pub fn label(self) -> &'static str { match self { Kind::Peak => "Peak", Kind::LowShelf => "Low shelf", Kind::HighShelf => "High shelf" } }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub kind: Kind,
    pub frequency: f64,
    pub gain: f64,
    pub q: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool { true }

impl Band {
    pub fn peak(frequency: f64, gain: f64, q: f64) -> Self { Self { kind: Kind::Peak, frequency, gain, q, enabled: true } }

    /// RBJ biquad, identical to Equalizer APO's BiQuad (Q form) so the graph predicts playback.
    pub fn coefficients(&self, rate: f64) -> [f64; 5] {
        let a = 10_f64.powf(self.gain / 40.0);
        let w = 2.0 * PI * self.frequency / rate;
        let (sn, cs) = w.sin_cos();
        let alpha = sn / (2.0 * self.q);
        let beta = 2.0 * a.sqrt() * alpha;
        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            Kind::Peak => (1.0 + alpha * a, -2.0 * cs, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cs, 1.0 - alpha / a),
            Kind::LowShelf => (a * ((a + 1.0) - (a - 1.0) * cs + beta), 2.0 * a * ((a - 1.0) - (a + 1.0) * cs), a * ((a + 1.0) - (a - 1.0) * cs - beta),
                (a + 1.0) + (a - 1.0) * cs + beta, -2.0 * ((a - 1.0) + (a + 1.0) * cs), (a + 1.0) + (a - 1.0) * cs - beta),
            Kind::HighShelf => (a * ((a + 1.0) + (a - 1.0) * cs + beta), -2.0 * a * ((a - 1.0) + (a + 1.0) * cs), a * ((a + 1.0) + (a - 1.0) * cs - beta),
                (a + 1.0) - (a - 1.0) * cs + beta, 2.0 * ((a - 1.0) - (a + 1.0) * cs), (a + 1.0) - (a - 1.0) * cs - beta),
        };
        [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
    }

    pub fn db_at(&self, f: f64, rate: f64) -> f64 {
        if !self.enabled { return 0.0; }
        let c = self.coefficients(rate);
        let z = Complex64::from_polar(1.0, -2.0 * PI * f / rate);
        20.0 * ((c[0] + c[1] * z + c[2] * z * z) / (1.0 + c[3] * z + c[4] * z * z)).norm().log10()
    }

    pub fn db(&self, f: f64) -> f64 { self.db_at(f, RATE) }

    /// Equalizer APO filter line (without the "Filter n:" prefix).
    pub fn apo(&self) -> String {
        let kind = match self.kind { Kind::Peak => "PK", Kind::LowShelf => "LSC", Kind::HighShelf => "HSC" };
        // Two decimals for Fc, never three: Equalizer APO multiplies "ddd.ddd" frequencies by 1000
        // (it reads them as Room EQ Wizard thousands separators), which silences playback.
        format!("ON {kind} Fc {:.2} Hz Gain {:.3} dB Q {:.4}", self.frequency, self.gain, self.q)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.frequency.is_finite() && FREQ_RANGE.contains(&self.frequency), "EQ band frequency must be 20 Hz–20 kHz");
        ensure!(self.gain.is_finite() && GAIN_RANGE.contains(&self.gain), "EQ band gain must be between -24 and +12 dB");
        ensure!(self.q.is_finite() && Q_RANGE.contains(&self.q), "EQ band Q must be between 0.1 and 10");
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManualEq {
    pub link: bool,
    pub left: Vec<Band>,
    pub right: Vec<Band>,
}

impl Default for ManualEq { fn default() -> Self { Self { link: true, left: vec![], right: vec![] } } }

impl ManualEq {
    pub fn bands(&self, right: bool) -> &[Band] { if right && !self.link { &self.right } else { &self.left } }
    pub fn bands_mut(&mut self, right: bool) -> &mut Vec<Band> { if right && !self.link { &mut self.right } else { &mut self.left } }
    /// Keeps the right channel identical to the left while linked.
    pub fn sync(&mut self) { if self.link { self.right = self.left.clone(); } }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EqConfig {
    pub version: u32,
    pub output_id: String,
    pub output_name: String,
    #[serde(default)]
    pub room: Option<Profile>,
    #[serde(default = "yes")]
    pub room_enabled: bool,
    #[serde(default)]
    pub manual: ManualEq,
}

impl Default for EqConfig {
    fn default() -> Self {
        Self { version: EQ_VERSION, output_id: String::new(), output_name: String::new(), room: None, room_enabled: true, manual: ManualEq::default() }
    }
}

impl EqConfig {
    pub fn from_profile(profile: Profile) -> Self {
        Self { output_id: profile.output_id.clone(), output_name: profile.output_name.clone(), room: Some(profile), ..Self::default() }
    }

    pub fn is_demo(&self) -> bool { self.room.as_ref().is_some_and(|p| p.demo) }

    pub fn measurement(&self, right: bool) -> Option<&Measurement> { self.room.as_ref().map(|p| if right { &p.right } else { &p.left }) }

    /// Every active filter of one speaker, room correction first.
    pub fn filters(&self, right: bool) -> Vec<Band> {
        let mut out: Vec<Band> = match self.measurement(right) {
            Some(m) if self.room_enabled => m.filters.iter().map(|f| Band::peak(f.frequency, f.gain, f.q)).collect(),
            _ => vec![],
        };
        out.extend(self.manual.bands(right).iter().filter(|b| b.enabled).cloned());
        out
    }

    /// Total EQ response in dB for one speaker (excluding the headroom preamp).
    pub fn response(&self, right: bool, f: f64) -> f64 {
        let room: f64 = match self.measurement(right) { Some(m) if self.room_enabled => m.correction(f), _ => 0.0 };
        room + self.manual.bands(right).iter().map(|b| b.db(f)).sum::<f64>()
    }

    /// Shared preamp: 1 dB headroom plus the largest boost of either speaker, so EQ cannot clip.
    pub fn preamp(&self) -> f64 {
        let peak = log_grid(20.0, 20_000.0, 1.01).iter().flat_map(|&f| [self.response(false, f), self.response(true, f)]).fold(0.0, f64::max);
        -((1.0 + peak) * 10.0).ceil() / 10.0
    }

    pub fn validate(&self, apply: bool) -> Result<()> {
        ensure!(self.version == EQ_VERSION, "Unsupported RoomTune EQ version");
        ensure!(self.output_name.len() <= 4096 && self.output_id.len() <= 4096, "EQ metadata is too long");
        if let Some(room) = &self.room { room.validate(apply && self.room_enabled)?; }
        ensure!(!apply || !self.is_demo(), "Demo data cannot be applied. Run a microphone calibration first");
        if apply { endpoint_guid(&self.output_id).context("Choose a speaker output first")?; }
        for bands in [&self.manual.left, &self.manual.right] {
            ensure!(bands.len() <= MAX_BANDS, "At most {MAX_BANDS} manual bands per speaker");
            for b in bands { b.validate()?; }
        }
        Ok(())
    }

    /// Loads a RoomTune file: a full EQ (version 3) or a room measurement profile (version 2).
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(fs::metadata(path)?.len() <= 4_000_000, "RoomTune file is too large");
        let text = fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&text).context("Could not read RoomTune JSON")?;
        let config = if value.get("version").and_then(|v| v.as_u64()) == Some(crate::profile::PROFILE_VERSION as u64) {
            Self::from_profile(serde_json::from_value(value).context("Could not read RoomTune profile")?)
        } else {
            serde_json::from_value(value).context("Could not read RoomTune EQ file")?
        };
        config.validate(false)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate(false)?;
        atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}
