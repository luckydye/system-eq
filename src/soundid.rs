//! Import of a Sonarworks SoundID Reference measurement (`.swproj`).
//!
//! The project's main payload is encrypted, but it carries an unencrypted `PEQb` part with the
//! averaged, mic-calibrated measured response of each speaker: blocks of little-endian `f32`
//! (frequency Hz, level dB) pairs from 20 Hz upward, left first, then right. RoomTune resamples
//! those curves onto its analysis grid and fits its own correction to them.
use crate::dsp::{Measurement, analysis_grid, normalize};
use anyhow::{Context, Result, ensure};
use std::path::Path;

/// One measured curve as (frequency, dB) points.
pub type Curve = Vec<(f64, f64)>;

fn f32_at(bytes: &[u8], at: usize) -> Option<f32> { bytes.get(at..at + 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])) }

/// Finds the measured-response curves in a `.swproj` file's bytes.
pub fn curves(bytes: &[u8]) -> Result<Vec<Curve>> {
    let start = bytes.windows(4).position(|w| w == b"PEQb").context("Not a SoundID Reference project (no measurement data found)")?;
    let mut found = vec![];
    let mut at = start + 4;
    while at + 16 <= bytes.len() {
        let (Some(first), Some(second)) = (f32_at(bytes, at), f32_at(bytes, at + 8)) else { break; };
        if first == 20.0 && second > 20.0 && second < 21.0 {
            let mut curve = vec![];
            let mut prev = 0.0;
            let mut j = at;
            while let (Some(freq), Some(db)) = (f32_at(bytes, j), f32_at(bytes, j + 4)) {
                let (freq, db) = (freq as f64, db as f64);
                if !(freq > prev && freq < 30_000.0 && db.is_finite() && db.abs() < 200.0) { break; }
                curve.push((freq, db));
                prev = freq;
                j += 8;
            }
            if curve.len() >= 100 { found.push(curve); at = j; continue; }
        }
        at += 1;
    }
    Ok(found)
}

/// Linear interpolation of a curve in log-frequency.
fn level_at(curve: &[(f64, f64)], f: f64) -> f64 {
    let i = curve.partition_point(|p| p.0 < f);
    if i == 0 { return curve[0].1; }
    if i == curve.len() { return curve[i - 1].1; }
    let ((f0, d0), (f1, d1)) = (curve[i - 1], curve[i]);
    d0 + (f / f0).ln() / (f1 / f0).ln() * (d1 - d0)
}

/// Converts an imported curve to a RoomTune measurement (SNR unknown: assumed clean).
pub fn to_measurement(curve: &[(f64, f64)]) -> Measurement {
    let frequencies = analysis_grid();
    let mut response: Vec<f64> = frequencies.iter().map(|&f| level_at(curve, f)).collect();
    normalize(&frequencies, &mut response);
    Measurement { sample_rate: 48_000, snr: vec![60.0; frequencies.len()], frequencies, response,
        peak_db: -20.0, noise_db: -80.0, filters: vec![], range: None }
}

/// Reads the left and right measured responses from a SoundID Reference project.
pub fn load(path: &Path) -> Result<(Measurement, Measurement)> {
    ensure!(std::fs::metadata(path)?.len() <= 64_000_000, "File is too large for a SoundID project");
    let bytes = std::fs::read(path)?;
    let curves = curves(&bytes)?;
    ensure!(curves.len() >= 2, "This SoundID project has no stereo measurement RoomTune can read (found {} curves)", curves.len());
    Ok((to_measurement(&curves[0]), to_measurement(&curves[1])))
}
