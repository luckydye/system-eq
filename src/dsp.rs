use anyhow::{Result, bail, ensure};
use rustfft::{FftPlanner, num_complex::Complex64};
use serde::{Deserialize, Serialize};
use std::{f64::consts::PI, path::Path};

pub const SWEEP_SECONDS: f64 = 10.0;
pub const MAX_FILTERS: usize = 16;
/// Bins below this SNR are too close to background noise to correct.
pub const MIN_SNR: f64 = 15.0;
/// Correction limits: boosts stay modest (they cost headroom and excite the room), cuts can go deeper.
pub const MAX_BOOST: f64 = 6.0;
pub const MAX_CUT: f64 = -12.0;
/// Response analysis grid.
pub const GRID_LOW: f64 = 20.0;
pub const GRID_HIGH: f64 = 18_000.0;

pub fn analysis_grid() -> Vec<f64> { log_grid(GRID_LOW, GRID_HIGH, 2_f64.powf(1.0 / 24.0)) }

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
    /// Frequency span the correction was fitted over (the speaker's usable range).
    #[serde(default)]
    pub range: Option<(f64, f64)>,
}

/// Fractional-octave smoothing width: fine in the bass, where room modes are narrow and audible,
/// broader in the treble, where the ear integrates (as in psychoacoustic smoothing).
pub fn smoothing_octaves(f: f64) -> f64 {
    let t = ((f / 200.0).log2() / 20_f64.log2()).clamp(0.0, 1.0);
    1.0 / 12.0 + t * (1.0 / 3.0 - 1.0 / 12.0)
}

/// Power-domain fractional-octave smoothing of a log-spaced response.
pub fn smooth(frequencies: &[f64], db: &[f64], octaves: impl Fn(f64) -> f64) -> Vec<f64> {
    frequencies.iter().map(|&f| {
        let half = 2_f64.powf(octaves(f) / 2.0);
        let (sum, count) = frequencies.iter().zip(db).filter(|(x, _)| (f / half..=f * half).contains(*x))
            .fold((0.0, 0usize), |(s, n), (_, d)| (s + 10_f64.powf(d / 10.0), n + 1));
        10.0 * (sum / count.max(1) as f64).max(1e-30).log10()
    }).collect()
}

fn median_in(frequencies: &[f64], values: &[f64], low: f64, high: f64) -> f64 {
    let mut v: Vec<f64> = frequencies.iter().zip(values).filter(|(f, _)| (low..=high).contains(*f)).map(|(_, &x)| x).collect();
    v.sort_by(f64::total_cmp);
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

/// Shifts a response so its 500 Hz–2 kHz median sits at 0 dB.
pub fn normalize(frequencies: &[f64], response: &mut [f64]) {
    let median = median_in(frequencies, response, 500.0, 2000.0);
    for r in response { *r -= median; }
}

/// Power average of several measurements taken on the same grid (multi-position averaging).
pub fn average(measurements: &[&Measurement]) -> Result<Measurement> {
    let first = measurements.first().ok_or_else(|| anyhow::anyhow!("No measurements to average"))?;
    ensure!(measurements.iter().all(|m| m.frequencies == first.frequencies), "Measurements use different frequency grids");
    let n = measurements.len() as f64;
    let mut response: Vec<f64> = (0..first.frequencies.len()).map(|i| {
        10.0 * (measurements.iter().map(|m| 10_f64.powf(m.response[i] / 10.0)).sum::<f64>() / n).log10()
    }).collect();
    normalize(&first.frequencies, &mut response);
    let snr = (0..first.frequencies.len()).map(|i| measurements.iter().map(|m| m.snr[i]).sum::<f64>() / n).collect();
    Ok(Measurement {
        sample_rate: first.sample_rate, frequencies: first.frequencies.clone(), response, snr,
        peak_db: measurements.iter().map(|m| m.peak_db).fold(f64::NEG_INFINITY, f64::max),
        noise_db: measurements.iter().map(|m| m.noise_db).fold(f64::NEG_INFINITY, f64::max),
        filters: vec![], range: None,
    })
}

impl Measurement {
    /// The response as used for display and fitting (see [`smoothing_octaves`]).
    pub fn smoothed(&self) -> Vec<f64> { smooth(&self.frequencies, &self.response, smoothing_octaves) }

    /// The speaker's usable band: where the half-octave-smoothed response stays within 6 dB of
    /// its midrange level. Boosting outside it only pushes power the speaker cannot reproduce.
    pub fn speaker_range(&self) -> (f64, f64) {
        let wide = smooth(&self.frequencies, &self.response, |_| 0.5);
        let reference = median_in(&self.frequencies, &wide, 300.0, 3000.0);
        let inside = |i: usize| wide[i] >= reference - 6.0;
        let f = &self.frequencies;
        let start = f.partition_point(|&x| x < 200.0).min(f.len() - 1);
        let mut low = start;
        while low > 0 && inside(low - 1) { low -= 1; }
        let start = f.partition_point(|&x| x < 2000.0).min(f.len() - 1);
        let mut high = start;
        while high + 1 < f.len() && inside(high + 1) { high += 1; }
        (f[low].max(GRID_LOW), f[high].min(GRID_HIGH))
    }
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
            ensure!(f.frequency.is_finite() && (GRID_LOW..=20_000.0).contains(&f.frequency)
                && f.gain.is_finite() && (MAX_CUT..=MAX_BOOST).contains(&f.gain)
                && f.q.is_finite() && (0.3..=10.0).contains(&f.q), "Filter exceeds correction limits");
        }
        ensure!(log_grid(20.0, 20_000.0, 1.01).iter().all(|&f| (MAX_CUT - 0.05..=MAX_BOOST + 0.05).contains(&self.correction(f))),
            "Combined correction exceeds -12/+6 dB");
        Ok(())
    }

    /// Plain-language notes about the fit, for the UI.
    pub fn fit_notes(&self) -> Vec<String> {
        let mut notes = vec![];
        if let Some((low, high)) = self.range {
            notes.push(format!("Corrected {low:.0} Hz – {:.1} kHz, the speaker's usable range.", high / 1000.0));
            let noisy: Vec<String> = noisy_ranges(&self.frequencies, &self.snr).iter().filter(|(lo, hi)| *hi >= low && *lo <= high)
                .map(|(lo, hi)| format!("{lo:.0}–{hi:.0} Hz")).collect();
            if !noisy.is_empty() { notes.push(format!("Too close to background noise at {}: left uncorrected.", noisy.join(", "))); }
        }
        if self.filters.is_empty() { notes.push("Response is already close to flat.".into()); }
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
    let frequencies = analysis_grid();
    let mut response = Vec::with_capacity(frequencies.len());
    let mut snr = Vec::with_capacity(frequencies.len());
    for &f in &frequencies {
        let signal = band(&rp, f, n, rate);
        let source = band(&sp, f, n, rate);
        let background = band(&np, f, n, rate) * recording.len() as f64 / noise.len() as f64 / 0.375;
        response.push(10.0 * (signal / source).log10() - mic.at(f));
        snr.push(10.0 * (signal / background.max(1e-25)).log10());
    }
    normalize(&frequencies, &mut response);
    let mut bass_snr: Vec<f64> = frequencies.iter().zip(&snr).filter(|(f, _)| (40.0..=300.0).contains(*f)).map(|(_, &r)| r).collect();
    bass_snr.sort_by(f64::total_cmp);
    if bass_snr[bass_snr.len() / 2] < 18.0 { bail!("Sweep is too close to background noise. Quiet the room, check the input, or raise the sweep level slightly"); }
    let result = Measurement {
        sample_rate: rate, frequencies, response, snr,
        peak_db: 20.0 * peak.log10(), noise_db: 10.0 * (noise.iter().map(|x| x * x).sum::<f64>() / noise.len() as f64).max(1e-20).log10(),
        filters: vec![], range: None,
    };
    result.validate()?;
    Ok(result)
}

/// Perceptual weight of a deviation from target (dB). Peaks are more audible than dips, so they
/// cost fully; dips cost half, and beyond -3 dB only linearly: a deep notch or roll-off edge that
/// EQ cannot fill must not stop a nearby resonance from being cut.
pub fn deviation_cost(e: f64) -> f64 {
    if e >= 0.0 { e * e } else if e >= -3.0 { 0.5 * e * e } else { 4.5 + 3.0 * (-e - 3.0) }
}

/// Fits a full-range correction toward a flat target: peaks are cut and dips raised (boosts are
/// broad and limited), within the speaker's usable range and only where the measurement is
/// clear of background noise. Works on the smoothed response, so single-position comb-filter
/// detail is not chased.
pub fn fit(m: &mut Measurement) {
    m.filters.clear();
    let (low, high) = m.speaker_range();
    m.range = Some((low, high));
    let target = m.smoothed();
    let (f, rate, n) = (&m.frequencies, m.sample_rate, m.frequencies.len());
    let active: Vec<bool> = (0..n).map(|i| (low..=high).contains(&f[i]) && m.snr[i] >= MIN_SNR).collect();
    if active.iter().filter(|a| **a).count() < 8 { return; }
    let curve = |flt: &Filter| -> Vec<f64> { f.iter().map(|&x| flt.db(x, rate)).collect() };
    // No boosting (beyond a 1 dB skirt) outside the fitted range or in noisy bins.
    let allowed = |i: usize, t: f64| (MAX_CUT..=MAX_BOOST).contains(&t) && (active[i] || t <= 1.0);
    let cost = |total: &[f64]| -> f64 { (0..n).filter(|&i| active[i]).map(|i| deviation_cost(target[i] + total[i])).sum() };
    let edge = 2_f64.powf(1.0 / 3.0);
    let boost_ok = |flt: &Filter| flt.gain <= 0.0 || (flt.q <= 2.0 && flt.frequency >= low * edge && flt.frequency <= high / edge);

    // Candidate bank, computed once.
    let mut candidates = vec![];
    for &frequency in &log_grid(low, high, 2_f64.powf(1.0 / 12.0)) {
        for q in [0.5, 0.7, 1.0, 1.4, 2.0, 3.0, 4.5, 6.0] {
            for gain in [-12.0, -9.0, -6.0, -4.0, -2.5, -1.5, 1.5, 2.5, 4.0, 6.0] {
                let flt = Filter { frequency, gain, q };
                if boost_ok(&flt) { candidates.push((curve(&flt), flt)); }
            }
        }
    }

    // Greedy selection: add the filter that lowers the error most, while it still helps clearly.
    let mut total = vec![0.0; n];
    let mut curves: Vec<Vec<f64>> = vec![];
    let mut current = cost(&total);
    for _ in 0..MAX_FILTERS {
        let mut best: Option<(usize, f64)> = None;
        'candidates: for (k, (c, _)) in candidates.iter().enumerate() {
            let mut trial = 0.0;
            for i in 0..n {
                let t = total[i] + c[i];
                if !allowed(i, t) { continue 'candidates; }
                if active[i] { trial += deviation_cost(target[i] + t); }
            }
            if best.is_none_or(|(_, b)| trial < b) { best = Some((k, trial)); }
        }
        // About 1 dB² across a third of an octave: below that a filter is not worth adding.
        let Some((k, trial)) = best.filter(|(_, trial)| current - trial >= 8.0) else { break; };
        for (t, c) in total.iter_mut().zip(&candidates[k].0) { *t += c; }
        curves.push(candidates[k].0.clone());
        m.filters.push(candidates[k].1.clone());
        current = trial;
    }

    // Continuous refinement of each filter's frequency, gain and Q.
    #[allow(clippy::needless_range_loop)] // `j` indexes both the filters and their cached curves
    for _ in 0..60 {
        let mut improved = false;
        for j in 0..m.filters.len() {
            for (df, dg, dq) in [(1.03, 0.0, 1.0), (1.0 / 1.03, 0.0, 1.0), (1.0, 0.25, 1.0), (1.0, -0.25, 1.0), (1.0, 0.0, 1.12), (1.0, 0.0, 1.0 / 1.12)] {
                let old = &m.filters[j];
                let flt = Filter { frequency: (old.frequency * df).clamp(GRID_LOW, 20_000.0), gain: (old.gain + dg).clamp(MAX_CUT, MAX_BOOST), q: (old.q * dq).clamp(0.3, 10.0) };
                if !boost_ok(&flt) { continue; }
                let c = curve(&flt);
                let trial: Vec<f64> = (0..n).map(|i| total[i] - curves[j][i] + c[i]).collect();
                if !trial.iter().enumerate().all(|(i, &t)| allowed(i, t)) { continue; }
                let tc = cost(&trial);
                if tc < current - 1e-6 { current = tc; total = trial; curves[j] = c; m.filters[j] = flt; improved = true; }
            }
        }
        if !improved { break; }
    }
    m.filters.retain(|f| f.gain.abs() >= 0.3);

    // Check between measured bins, where overlapping filters can exceed the limits.
    for _ in 0..100 {
        if log_grid(20.0, 20_000.0, 1.01).iter().all(|&f| (MAX_CUT + 0.05..=MAX_BOOST - 0.05).contains(&m.correction(f))) { break; }
        for filter in &mut m.filters { filter.gain *= 0.98; }
    }
}

pub fn demo_measurement(peak: f64) -> Measurement {
    let frequencies = analysis_grid();
    let response = frequencies.iter().map(|f| 8.0 * (-((f / peak).ln() / 0.21).powi(2)).exp()
        + 4.0 * (-((f / 170.0).ln() / 0.16).powi(2)).exp()
        - 5.0 * (-((f / 115.0).ln() / 0.10).powi(2)).exp() + 0.4 * (f.ln() * 7.0).sin()
        - 24.0 * (1.0 - (f / 45.0).min(1.0)).max(0.0) - (f / 12_000.0).max(1.0).log2() * 12.0).collect();
    let mut m = Measurement { sample_rate: 48_000, snr: vec![45.0; frequencies.len()], frequencies,
        response, peak_db: -18.0, noise_db: -72.0, filters: vec![], range: None };
    fit(&mut m); m
}
