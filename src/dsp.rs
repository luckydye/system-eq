use anyhow::{Result, bail, ensure};
use rustfft::{FftPlanner, num_complex::Complex64};
use serde::{Deserialize, Serialize};
use std::{f64::consts::PI, path::Path};

pub const SWEEP_SECONDS: f64 = 10.0;
pub const MAX_FILTERS: usize = 8;
/// Bins below this SNR are too close to background noise to justify a cut.
pub const MIN_SNR: f64 = 15.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Filter {
    pub frequency: f64,
    pub gain: f64,
    pub q: f64,
}

impl Filter {
    pub fn coefficients(&self, sample_rate: u32) -> [f64; 5] {
        let a = 10_f64.powf(self.gain / 40.0);
        let w = 2.0 * PI * self.frequency / sample_rate as f64;
        let alpha = w.sin() / (2.0 * self.q);
        let a0 = 1.0 + alpha / a;
        [(1.0 + alpha * a) / a0, -2.0 * w.cos() / a0,
         (1.0 - alpha * a) / a0, -2.0 * w.cos() / a0, (1.0 - alpha / a) / a0]
    }

    pub fn db(&self, frequency: f64, sample_rate: u32) -> f64 {
        let c = self.coefficients(sample_rate);
        let z = Complex64::from_polar(1.0, -2.0 * PI * frequency / sample_rate as f64);
        20.0 * ((c[0] + c[1] * z + c[2] * z * z) / (1.0 + c[3] * z + c[4] * z * z)).norm().log10()
    }

    pub fn process(&self, input: &[f64], sample_rate: u32) -> Vec<f64> {
        let c = self.coefficients(sample_rate);
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        input.iter().map(|&x| {
            let y = c[0] * x + c[1] * x1 + c[2] * x2 - c[3] * y1 - c[4] * y2;
            x2 = x1; x1 = x; y2 = y1; y1 = y; y
        }).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Measurement {
    pub sample_rate: u32,
    pub frequencies: Vec<f64>,
    pub response: Vec<f64>,
    pub snr: Vec<f64>,
    pub peak_db: f64,
    pub noise_db: f64,
    pub filters: Vec<Filter>,
}

impl Measurement {
    pub fn correction(&self, f: f64) -> f64 {
        self.filters.iter().map(|x| x.db(f, self.sample_rate)).sum()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!((44_100..=192_000).contains(&self.sample_rate), "Unsupported sample rate in profile");
        let n = self.frequencies.len();
        ensure!((20..=1000).contains(&n) && self.response.len() == n && self.snr.len() == n, "Invalid response data");
        ensure!(self.frequencies.windows(2).all(|w| w[0] < w[1]), "Response frequencies are not ordered");
        ensure!(self.frequencies.iter().all(|x| x.is_finite() && (20.0..=20_000.0).contains(x)), "Invalid frequency");
        ensure!(self.response.iter().all(|x| x.is_finite() && x.abs() <= 200.0) && self.snr.iter().all(|x| x.is_finite()), "Invalid response or SNR");
        ensure!(self.peak_db.is_finite() && self.noise_db.is_finite(), "Invalid microphone levels");
        ensure!(self.filters.len() <= MAX_FILTERS, "Too many filters");
        for f in &self.filters {
            ensure!(f.frequency.is_finite() && (30.0..=300.0).contains(&f.frequency)
                && f.gain.is_finite() && (-9.0..=0.0).contains(&f.gain)
                && f.q.is_finite() && (0.5..=8.0).contains(&f.q), "Filter exceeds correction limits");
        }
        ensure!(log_grid(20.0, 20_000.0, 1.01).iter().all(|&f| self.correction(f) >= -12.05), "Combined cuts exceed 12 dB");
        Ok(())
    }

    /// Plain-language reasons the fit made few or no cuts, for the UI.
    pub fn fit_notes(&self) -> Vec<String> {
        let bass: Vec<usize> = (0..self.frequencies.len()).filter(|&i| (30.0..=300.0).contains(&self.frequencies[i])).collect();
        let mut notes = vec![];
        let noisy = noisy_ranges(&self.frequencies, &self.snr);
        let noisy_bins = bass.iter().filter(|&&i| self.snr[i] < MIN_SNR).count();
        if noisy_bins > 0 {
            let spans: Vec<String> = noisy.iter().filter(|(lo, hi)| *hi >= 30.0 && *lo <= 300.0).map(|(lo, hi)| format!("{lo:.0}–{hi:.0} Hz")).collect();
            notes.push(format!("Too close to background noise at {}: no cuts there.", spans.join(", ")));
        }
        let mut levels: Vec<f64> = bass.iter().map(|&i| self.response[i]).collect();
        levels.sort_by(f64::total_cmp);
        let median = levels[levels.len() / 2];
        if median < -6.0 {
            notes.push(format!("Bass sits {:.0} dB below the midrange. Cuts cannot raise it, so they stay shallow.", -median));
        }
        if self.filters.is_empty() {
            let peak = bass.iter().filter(|&&i| self.snr[i] >= MIN_SNR).map(|&i| self.response[i]).fold(f64::NEG_INFINITY, f64::max);
            if peak.is_finite() { notes.push(format!("Highest bass peak is {peak:+.1} dB: too small to cut without deepening its neighbours.")); }
        }
        notes
    }
}

/// Frequency spans (low, high) where the SNR is below [`MIN_SNR`].
pub fn noisy_ranges(frequencies: &[f64], snr: &[f64]) -> Vec<(f64, f64)> {
    let mut spans: Vec<(f64, f64)> = vec![];
    let mut previous = false;
    for (&f, &s) in frequencies.iter().zip(snr) {
        let noisy = s < MIN_SNR;
        if noisy && previous { spans.last_mut().unwrap().1 = f; } else if noisy { spans.push((f, f)); }
        previous = noisy;
    }
    spans
}

#[derive(Clone, Debug)]
pub struct MicCalibration {
    pub name: String,
    points: Vec<(f64, f64)>,
}

impl Default for MicCalibration {
    fn default() -> Self { Self { name: "Uncalibrated microphone".into(), points: vec![] } }
}

impl MicCalibration {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(std::fs::metadata(path)?.len() <= 1_000_000, "Calibration file is too large");
        Self::parse(&std::fs::read_to_string(path)?, &path.file_name().unwrap_or_default().to_string_lossy())
    }

    pub fn parse(text: &str, name: &str) -> Result<Self> {
        let mut points = vec![];
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with(['#', '*', '"']) { continue; }
            let mut p = t.split(|c: char| c.is_whitespace() || c == ',' || c == ';').filter(|x| !x.is_empty());
            if let (Some(f), Some(db)) = (p.next(), p.next())
                && let (Ok(f), Ok(db)) = (f.parse::<f64>(), db.parse::<f64>())
                && f.is_finite() && db.is_finite() && (10.0..=40_000.0).contains(&f) && db.abs() <= 40.0 {
                points.push((f, db));
            }
        }
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points.dedup_by(|a, b| a.0 == b.0);
        ensure!(points.len() >= 5 && points[0].0 <= 30.0 && points.last().unwrap().0 >= 2000.0,
            "Use a frequency/response calibration file covering at least 30 Hz–2 kHz");
        Ok(Self { name: name.into(), points })
    }

    pub fn at(&self, f: f64) -> f64 {
        if self.points.is_empty() { return 0.0; }
        let i = self.points.partition_point(|p| p.0 < f);
        if i == 0 { return self.points[0].1; }
        if i == self.points.len() { return self.points[i - 1].1; }
        let (f0, d0) = self.points[i - 1]; let (f1, d1) = self.points[i];
        d0 + (f / f0).ln() / (f1 / f0).ln() * (d1 - d0)
    }
}

pub fn log_grid(low: f64, high: f64, step: f64) -> Vec<f64> {
    let mut result = Vec::new(); let mut f = low;
    while f <= high { result.push(f); f *= step; }
    result
}

pub fn sweep(rate: u32, level_db: f64) -> Vec<f64> {
    let k = SWEEP_SECONDS / (20_000_f64 / 20.0).ln();
    let amp = 10_f64.powf(level_db / 20.0);
    (0..(rate as f64 * SWEEP_SECONDS) as usize).map(|i| {
        let t = i as f64 / rate as f64;
        let fade = (t / 0.05).min((SWEEP_SECONDS - t) / 0.05).clamp(0.0, 1.0);
        amp * 0.5 * (1.0 - (PI * fade).cos()) * (2.0 * PI * 20.0 * k * ((t / k).exp() - 1.0)).sin()
    }).collect()
}

fn power(data: &[f64], n: usize) -> Vec<f64> {
    let mut buffer = vec![Complex64::default(); n];
    for (dst, &x) in buffer.iter_mut().zip(data) { dst.re = x; }
    FftPlanner::new().plan_fft_forward(n).process(&mut buffer);
    buffer[..=n / 2].iter().map(|x| x.norm_sqr()).collect()
}

fn band(p: &[f64], frequency: f64, n: usize, rate: u32) -> f64 {
    let edge = 2_f64.powf(1.0 / 12.0);
    let low = ((frequency / edge * n as f64 / rate as f64).ceil() as usize).max(1);
    let high = ((frequency * edge * n as f64 / rate as f64).floor() as usize).min(p.len() - 1);
    p.get(low..=high).map(|bins| bins.iter().sum::<f64>()).unwrap_or(0.0).max(1e-25)
}

pub fn analyze(recording: &[f64], reference: &[f64], noise: &[f64], rate: u32, mic: &MicCalibration) -> Result<Measurement> {
    ensure!((44_100..=192_000).contains(&rate), "Use a sample rate between 44.1 and 192 kHz");
    ensure!(recording.len() >= reference.len() && noise.len() >= rate as usize / 2, "Microphone recording is incomplete");
    ensure!(recording.len() <= rate as usize * 20, "Microphone recording exceeds the capture limit");
    ensure!(recording.iter().chain(noise).chain(reference).all(|x| x.is_finite()), "Invalid audio samples received");
    let peak = recording.iter().map(|x| x.abs()).fold(0.0, f64::max);
    ensure!(peak < 0.999, "Microphone clipped. Lower input gain or speaker volume, then measure again");
    ensure!(peak >= 0.0003, "No usable microphone signal. Check the selected input channel and microphone gain");
    let n = recording.len().max(reference.len()).next_power_of_two();
    let rp = power(recording, n); let sp = power(reference, n);
    let window: Vec<_> = noise.iter().enumerate().map(|(i, x)| x * 0.5 * (1.0 - (2.0 * PI * i as f64 / (noise.len() - 1) as f64).cos())).collect();
    let np = power(&window, n);
    let frequencies = log_grid(25.0, 16_000.0, 2_f64.powf(1.0 / 24.0));
    let mut response = Vec::with_capacity(frequencies.len());
    let mut snr = Vec::with_capacity(frequencies.len());
    for &f in &frequencies {
        let signal = band(&rp, f, n, rate);
        let source = band(&sp, f, n, rate);
        let background = band(&np, f, n, rate) * recording.len() as f64 / noise.len() as f64 / 0.375;
        response.push(10.0 * (signal / source).log10() - mic.at(f));
        snr.push(10.0 * (signal / background.max(1e-25)).log10());
    }
    let mut baseline: Vec<f64> = frequencies.iter().zip(&response).filter(|(f, _)| (500.0..=2000.0).contains(*f)).map(|(_, &r)| r).collect();
    baseline.sort_by(f64::total_cmp);
    let median = baseline[baseline.len() / 2];
    let mut bass_snr: Vec<f64> = frequencies.iter().zip(&snr).filter(|(f, _)| (40.0..=300.0).contains(*f)).map(|(_, &r)| r).collect();
    bass_snr.sort_by(f64::total_cmp);
    if bass_snr[bass_snr.len() / 2] < 18.0 { bail!("Sweep is too close to background noise. Quiet the room, check the input, or raise the sweep level slightly"); }
    let result = Measurement {
        sample_rate: rate, frequencies, response: response.iter().map(|x| x - median).collect(), snr,
        peak_db: 20.0 * peak.log10(), noise_db: 10.0 * (noise.iter().map(|x| x * x).sum::<f64>() / noise.len() as f64).max(1e-20).log10(), filters: vec![],
    };
    result.validate()?;
    Ok(result)
}

pub fn fit(m: &mut Measurement) {
    m.filters.clear();
    let mut total = vec![0.0; m.response.len()];
    let centers = log_grid(30.0, 300.0, 2_f64.powf(1.0 / 12.0));
    // Peaks cost quadratically and dips three times as much, but only down to -3 dB: below that the
    // penalty grows linearly. A bass region already far under target (e.g. speaker roll-off) would
    // otherwise make every cut's skirt so expensive that no resonance is ever cut.
    let cost = |x: f64| if x >= 0.0 { x * x } else if x >= -3.0 { 3.0 * x * x } else { 24.0 - x };
    // Cache candidate responses once: the fit performs no repeated trigonometry.
    let mut candidates = vec![];
    for &frequency in &centers {
        for q in [0.7, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0] {
            for gain in [-1.0, -2.0, -3.0, -4.5, -6.0, -9.0] {
                let filter = Filter { frequency, gain, q };
                let curve: Vec<_> = m.frequencies.iter().map(|&f| filter.db(f, m.sample_rate)).collect();
                candidates.push((filter, curve));
            }
        }
    }
    for _ in 0..MAX_FILTERS {
        let mut best = None; let mut best_improvement = 0.25;
        for (candidate, (_, curve)) in candidates.iter().enumerate() {
            let mut improvement = 0.0; let mut valid = true;
            for (i, &cut) in curve.iter().enumerate() {
                if total[i] + cut < -12.0 { valid = false; break; }
                if !(30.0..=300.0).contains(&m.frequencies[i]) { continue; }
                if m.snr[i] < MIN_SNR { if cut < -0.5 { valid = false; break; } continue; }
                let before = m.response[i] + total[i];
                improvement += cost(before) - cost(before + cut);
            }
            if valid && improvement > best_improvement { best = Some(candidate); best_improvement = improvement; }
        }
        let Some(best) = best else { break; };
        m.filters.push(candidates[best].0.clone());
        for (total, cut) in total.iter_mut().zip(&candidates[best].1) { *total += cut; }
    }
    // Check between measured bins, where overlapping filters can have deeper minima.
    for _ in 0..100 {
        if log_grid(20.0, 20_000.0, 1.01).iter().all(|&f| m.correction(f) >= -11.95) { break; }
        for filter in &mut m.filters { filter.gain *= 0.98; }
    }
}

pub fn demo_measurement(peak: f64) -> Measurement {
    let frequencies = log_grid(25.0, 16_000.0, 2_f64.powf(1.0 / 24.0));
    let response = frequencies.iter().map(|f| 8.0 * (-((f / peak).ln() / 0.21).powi(2)).exp()
        + 4.0 * (-((f / 170.0).ln() / 0.16).powi(2)).exp()
        - 5.0 * (-((f / 115.0).ln() / 0.10).powi(2)).exp() + 0.4 * (f.ln() * 7.0).sin()).collect();
    let mut m = Measurement { sample_rate: 48_000, snr: vec![45.0; frequencies.len()], frequencies,
        response, peak_db: -18.0, noise_db: -72.0, filters: vec![] };
    fit(&mut m); m
}
