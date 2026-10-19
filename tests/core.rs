//! Non-audio tests: no microphone, playback, installer, or system configuration is touched.
use room_tune::{apo::{self, Operation, Status}, audio::route_frame, dsp::{self, Filter, MicCalibration},
    engine::{self, Mode, Slot}, eq::{Band, EqConfig, Kind}, profile::{Profile, atomic_write}};
use std::fs;

const RATE: u32 = 48_000;

fn noise() -> Vec<f64> {
    // Deterministic LCG noise at roughly -126 dBFS.
    let mut state = 741_u64;
    (0..RATE).map(|_| { state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 1e-6 }).collect()
}

fn record(signal: &[f64], noise: &[f64]) -> Vec<f64> {
    let mut recording = vec![0.0; signal.len() + RATE as usize * 2];
    recording[RATE as usize..RATE as usize + signal.len()].copy_from_slice(signal);
    for (i, x) in recording.iter_mut().enumerate() { *x += noise[i % noise.len()]; }
    recording
}

fn nearest(m: &dsp::Measurement, f: f64) -> usize {
    (0..m.frequencies.len()).min_by(|&a, &b| (m.frequencies[a] - f).abs().total_cmp(&(m.frequencies[b] - f).abs())).unwrap()
}

fn test_profile() -> Profile {
    let sweep = dsp::sweep(RATE, -30.0); let noise = noise();
    let mut left = dsp::analyze(&record(&Filter { frequency: 80.0, gain: 8.0, q: 4.0 }.process(&sweep, RATE), &noise), &sweep, &noise, RATE, &MicCalibration::default()).unwrap();
    let mut right = dsp::analyze(&record(&sweep, &noise), &sweep, &noise, RATE, &MicCalibration::default()).unwrap();
    dsp::fit(&mut left); dsp::fit(&mut right);
    Profile { output_id: "{0.0.0.00000000}.{01234567-89ab-cdef-0123-456789abcdef}".into(), output_name: "Test output".into(),
        microphone_name: "Test".into(), calibration_name: "None".into(), demo: false, left, right, ..Profile::demo() }
}

#[test]
fn biquad() {
    let cut = Filter { frequency: 80.0, gain: -6.0, q: 3.0 };
    assert!((cut.db(80.0, RATE) + 6.0).abs() < 1e-8, "gain at center");
    assert!(cut.db(5000.0, RATE).abs() < 0.01, "high-frequency rejection");
}

#[test]
fn sweep_shape() {
    let sweep = dsp::sweep(RATE, -30.0);
    assert_eq!(sweep.len(), (RATE as f64 * dsp::SWEEP_SECONDS) as usize);
    assert!(sweep.iter().all(|x| x.abs() <= 10_f64.powf(-1.5) + 1e-8), "level cap");
    assert!(sweep[0].abs() < 1e-10 && sweep.last().unwrap().abs() < 1e-6, "fades");
}

#[test]
fn routing() {
    let mut frame = [9.0_f32; 4];
    route_frame(&mut frame, 1, 0.25);
    assert_eq!(frame, [0.0, 0.25, 0.0, 0.0]);
    let mut frame = [7_i16; 2];
    route_frame(&mut frame, 0, -0.5);
    assert_eq!(frame[1], 0);
    assert!(frame[0] < -16_000);
}

#[test]
fn recovers_and_corrects_resonance() {
    let m = test_profile().left;
    let at = nearest(&m, 80.0);
    assert!((6.8..8.5).contains(&m.response[at]), "recover known 8 dB resonance despite delay: {}", m.response[at]);
    let corrected = m.response[at] + m.correction(m.frequencies[at]);
    assert!(corrected.abs() < 2.0, "suppress resonance: {corrected}");
    assert!(!m.filters.is_empty() && m.filters.len() <= dsp::MAX_FILTERS, "bounded filter count");
    assert!(m.filters.iter().all(|f| f.gain < 0.0), "cut-only");
    m.validate().unwrap();
}

#[test]
fn flat_system_needs_no_correction() {
    let m = test_profile().right;
    assert!(m.response.iter().all(|x| x.abs() < 0.001), "flat transfer estimate");
    assert!(m.filters.is_empty());
}

#[test]
fn snr_matches_theory() {
    // Exponential sweep band energy over white noise: S/N = A² k rate² / (4 f L σ²).
    // Guards the noise-band scaling (record/noise length ratio, Hann power factor 0.375).
    let sweep = dsp::sweep(RATE, -30.0);
    let noise: Vec<_> = noise().iter().map(|x| x * 20.0).collect();
    let recording = record(&sweep, &noise);
    let m = dsp::analyze(&recording, &sweep, &noise, RATE, &MicCalibration::default()).unwrap();
    let variance = noise.iter().map(|x| x * x).sum::<f64>() / noise.len() as f64;
    let k = dsp::SWEEP_SECONDS / 1000_f64.ln();
    for f in [40.0, 100.0, 300.0, 1000.0] {
        let expected = 10.0 * (1.0 + 1e-3 * k * (RATE as f64).powi(2) / (4.0 * f * recording.len() as f64 * variance)).log10();
        let got = m.snr[nearest(&m, f)];
        assert!((got - expected).abs() < 4.0, "SNR at {f} Hz: {got:.1} dB, expected {expected:.1} dB");
    }
}

#[test]
fn cuts_peak_above_rolled_off_bass() {
    // Shaped like a real small-speaker measurement: bass 15-40 dB under the midrange below ~110 Hz,
    // a +3.5 dB resonance at 126 Hz, and noisy bins under 45 Hz. The old cost found nothing to cut.
    let frequencies = dsp::log_grid(25.0, 16_000.0, 2_f64.powf(1.0 / 24.0));
    let response = frequencies.iter().map(|&f| {
        let rolloff = -15.0 / (1.0 + (f / 95.0).powi(12)) - 25.0 / (1.0 + (f / 40.0).powi(8));
        rolloff + 3.5 * (-((f / 126.0).ln() / 0.06).powi(2)).exp()
    }).collect();
    let snr = frequencies.iter().map(|&f| if f < 45.0 { 5.0 } else { 35.0 }).collect();
    let mut m = dsp::Measurement { sample_rate: RATE, frequencies, response, snr, peak_db: -20.0, noise_db: -66.0, filters: vec![] };
    dsp::fit(&mut m);
    m.validate().unwrap();
    assert!(m.filters.iter().any(|f| (115.0..140.0).contains(&f.frequency)), "cut the 126 Hz resonance: {:?}", m.filters);
    let at = nearest(&m, 126.0);
    assert!(m.response[at] + m.correction(m.frequencies[at]) < 2.5, "resonance reduced");
    let low = nearest(&m, 35.0);
    assert!(m.correction(m.frequencies[low]) > -0.5, "no cuts in the noisy region");
    let notes = m.fit_notes().join(" ");
    assert!(notes.contains("background noise") && notes.contains("below the midrange"), "explains limits: {notes}");
}

#[test]
fn rejects_bad_recordings() {
    let sweep = dsp::sweep(RATE, -30.0); let noise = noise(); let mic = MicCalibration::default();
    let mut clipped = record(&sweep, &noise); clipped[100] = 1.0;
    assert!(dsp::analyze(&clipped, &sweep, &noise, RATE, &mic).is_err(), "clipped");
    assert!(dsp::analyze(&vec![0.0; clipped.len()], &sweep, &noise, RATE, &mic).is_err(), "silence");
    let loud: Vec<_> = noise.iter().map(|x| x * 1000.0).collect();
    let noisy: Vec<_> = (0..clipped.len()).map(|i| loud[i % loud.len()]).collect();
    assert!(dsp::analyze(&noisy, &sweep, &loud, RATE, &mic).is_err(), "noise only");
}

#[test]
fn mic_calibration() {
    let mic = MicCalibration::parse("\"Sens Factor = -0.5dB\"\n20 2\n40 2\n100 2\n1000 2\n20000 2\n", "mic.txt").unwrap();
    assert!((mic.at(300.0) - 2.0).abs() < 1e-10);
    let mic = MicCalibration::parse("20,0\n100;1\n1000\t2\n2000 3\n10000 4\n", "mixed").unwrap();
    assert!((mic.at(2000.0) - 3.0).abs() < 1e-10 && mic.at(5.0) == 0.0);
    assert!(MicCalibration::parse("100 1\n200 2\n", "short").is_err());
}

#[test]
fn demo_cannot_be_applied() {
    let demo = Profile::demo();
    demo.validate(false).unwrap();
    assert!(apo::export(&EqConfig::from_profile(demo)).is_err());
}

#[test]
fn shelves_match_rbj() {
    let low = Band { kind: Kind::LowShelf, frequency: 100.0, gain: 6.0, q: 0.707, enabled: true };
    assert!((low.db(20.0) - 6.0).abs() < 0.2 && low.db(5000.0).abs() < 0.05);
    assert!((low.db(100.0) - 3.0).abs() < 0.05, "half gain at center");
    let high = Band { kind: Kind::HighShelf, frequency: 4000.0, gain: -4.0, q: 0.707, enabled: true };
    assert!((high.db(18_000.0) + 4.0).abs() < 0.2 && high.db(50.0).abs() < 0.05);
    let off = Band { enabled: false, ..high.clone() };
    assert_eq!(off.db(18_000.0), 0.0);
    assert!(Band::peak(1000.0, 3.0, 1.0).apo().starts_with("ON PK Fc 1000.00 Hz Gain 3.000 dB"));
    assert!(high.apo().starts_with("ON HSC Fc 4000.00 Hz Gain -4.000 dB Q 0.7070"));
}

/// Equalizer APO's frequency parsing (filters/BiQuadFilterFactory.cpp `getFreq`): commas become
/// periods, and a value of 5+ characters with a period 4 places from the end is multiplied by
/// 1000, because Room EQ Wizard writes thousands separators like "1.000".
fn apo_parse_freq(s: &str) -> f64 {
    let s = s.replace(',', ".");
    let value: f64 = s.parse().unwrap();
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 5 && !s.contains(['e', 'E']) && chars[chars.len() - 4] == '.' { value * 1000.0 } else { value }
}

#[test]
fn exported_frequencies_survive_apo_parsing() {
    // Regression: "Fc 127.136 Hz" was read as 127136 Hz and silenced playback.
    let mut eq = EqConfig::from_profile(test_profile());
    for f in [20.0, 45.0, 127.136, 999.9994, 1010.0, 5320.0, 19_999.5] { eq.manual.left.push(Band::peak(f, -1.0, 1.0)); }
    eq.manual.sync();
    let text = apo::export(&eq).unwrap();
    let mut checked = 0;
    for line in text.lines().filter(|l| l.starts_with("Filter")) {
        let fc = line.split(" Fc ").nth(1).unwrap().split(" Hz").next().unwrap().trim();
        let parsed = apo_parse_freq(fc);
        assert!((20.0..=20_000.0).contains(&parsed), "{line} parses as {parsed} Hz in Equalizer APO");
        let written: f64 = fc.parse().unwrap();
        assert!((parsed - written).abs() < 1e-9, "{line}");
        checked += 1;
    }
    assert!(checked >= 14);
}

#[test]
fn manual_eq_export_and_headroom() {
    let mut eq = EqConfig::from_profile(test_profile());
    eq.manual.left.push(Band { kind: Kind::LowShelf, frequency: 80.0, gain: 4.0, q: 0.7, enabled: true });
    eq.manual.sync();
    let text = apo::export(&eq).unwrap();
    assert_eq!(text.matches("ON LSC Fc 80.00 Hz").count(), 2, "linked band on both speakers");
    assert!(eq.preamp() <= -4.9, "preamp covers the boost plus 1 dB: {}", eq.preamp());
    assert!(text.contains(&format!("Preamp: {:.1} dB", eq.preamp())));
    eq.room_enabled = false;
    assert!(!apo::export(&eq).unwrap().contains(" PK "), "room correction can be switched off");
    eq.manual.left.push(Band::peak(1000.0, 20.0, 1.0));
    assert!(eq.validate(true).is_err(), "reject excessive boost");
    let manual_only = EqConfig { output_id: "{0.0.0.00000000}.{01234567-89ab-cdef-0123-456789abcdef}".into(), ..EqConfig::default() };
    assert!(apo::export(&manual_only).unwrap().contains("Preamp: -1.0 dB"));
    assert!(apo::export(&EqConfig::default()).is_err(), "an output is required");
}

#[test]
fn eq_files_load_room_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let measurement = dir.path().join("measurement.roomtune");
    test_profile().save(&measurement).unwrap();
    let eq = EqConfig::load(&measurement).unwrap();
    assert!(eq.room.is_some() && eq.room_enabled && eq.manual.left.is_empty());
    let path = dir.path().join("eq.roomtune");
    let mut eq = eq; eq.manual.left.push(Band::peak(2000.0, -2.0, 2.0)); eq.manual.link = false;
    eq.save(&path).unwrap();
    let loaded = EqConfig::load(&path).unwrap();
    assert_eq!(loaded.manual, eq.manual);
    assert!(loaded.filters(true).len() < loaded.filters(false).len(), "unlinked right channel has no manual band");
}

#[test]
fn engine_mode_selection() {
    let has = |list: &'static [u32]| move |n: u32| list.contains(&n);
    assert_eq!(engine::choose_mode(has(&[1, 2]), false), Mode::LfxGfx);
    assert_eq!(engine::choose_mode(has(&[1, 2, 13]), false), Mode::SfxEfx);
    assert_eq!(engine::choose_mode(has(&[5, 6]), true), Mode::SfxMfx);
    assert_eq!(engine::choose_mode(has(&[]), false), Mode::SfxEfx);
    let originals = |s: Slot| match s { Slot::Sfx => "{SFX}".to_string(), Slot::Lfx => "{LFX}".into(), _ => "!VALUE".into() };
    assert_eq!(engine::child_apos(originals, Mode::SfxEfx), ("{SFX}".into(), String::new()));
    let only_legacy = |s: Slot| match s { Slot::Lfx => "{LFX}".to_string(), Slot::Gfx => "{GFX}".into(), _ => "!VALUE".into() };
    assert_eq!(engine::child_apos(only_legacy, Mode::SfxMfx), ("{LFX}".into(), "{GFX}".into()), "fall back to legacy slots");
    assert_eq!(engine::child_apos(|_| "!KEY".to_string(), Mode::LfxGfx), (String::new(), String::new()));
}

#[test]
fn profile_round_trip_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let p = test_profile(); p.validate(true).unwrap();
    let path = dir.path().join("profile.roomtune");
    p.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();
    assert_eq!(loaded.left.filters.len(), p.left.filters.len());
    assert_eq!(loaded.output_id, p.output_id);
    fs::write(&path, "not a profile").unwrap();
    assert!(Profile::load(&path).is_err());
    let mut unsafe_boost = p.clone();
    unsafe_boost.left.filters.push(Filter { frequency: 80.0, gain: 12.0, q: 3.0 });
    assert!(unsafe_boost.validate(true).is_err(), "reject imported boost");
}

#[test]
fn export_format() {
    let text = apo::export(&EqConfig::from_profile(test_profile())).unwrap();
    assert!(text.contains("Device: {01234567-89ab-cdef-0123-456789abcdef}"));
    assert!(text.contains("Channel: L") && text.contains("Channel: R") && text.contains("Preamp: -1.0 dB"), "cut-only room EQ needs only 1 dB headroom");
    assert!(text.lines().filter(|l| l.starts_with("Filter")).all(|l| !l.contains(',') && l.contains(" ON PK Fc ")));
    assert!(text.trim_end().ends_with("Device: all"), "reset scope for later config lines");
}

#[test]
fn integrate_preserves_config() {
    let original = "# User config\r\nPreamp: -2 dB\r\nInclude: personal.txt\r\n";
    let integrated = apo::integrate(original).unwrap();
    assert!(integrated.starts_with(original.trim_end()));
    assert_eq!(apo::integrate(&integrated).unwrap(), integrated, "idempotent");
    assert_eq!(integrated.matches(apo::BEGIN).count(), 1);
    let mut later = integrated.clone(); later.push_str("Preamp: -3 dB\r\n");
    assert!(apo::integrate(&later).unwrap().contains("Preamp: -3 dB"), "keep lines after block");
    assert!(apo::integrate(&format!("{original}{}", apo::BEGIN)).is_err(), "incomplete");
    assert!(apo::integrate(&format!("{integrated}{integrated}")).is_err(), "duplicated");
}

#[test]
fn atomic_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.txt");
    atomic_write(&path, b"first").unwrap();
    atomic_write(&path, b"second").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "second");
}

#[test]
fn apply_bypass_restore_on_fake_install() {
    let root = tempfile::tempdir().unwrap();
    let paths = apo::Paths::new(root.path());
    fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
    fs::write(&paths.config, "Preamp: -2 dB\r\n").unwrap();
    assert_eq!(apo::status_at(root.path()), Status::NotConfigured);
    assert!(apo::write_operation(root.path(), Operation::Restore, None).is_err(), "nothing to restore yet");

    let mut eq = EqConfig::from_profile(test_profile());
    apo::write_operation(root.path(), Operation::Apply, Some(&eq)).unwrap();
    assert_eq!(apo::status_at(root.path()), Status::Enabled);
    assert!(fs::read_to_string(&paths.config).unwrap().starts_with("Preamp: -2 dB"));
    let backups = || fs::read_dir(&paths.owned).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("config-backup-")).count();
    assert_eq!(backups(), 1);
    eq.manual.left.push(Band::peak(3000.0, -2.0, 1.5));
    apo::write_operation(root.path(), Operation::Apply, Some(&eq)).unwrap();
    assert_eq!(backups(), 1, "live re-apply leaves config.txt alone");
    assert!(fs::read_to_string(&paths.active).unwrap().contains("Fc 3000.00 Hz"));

    apo::write_operation(root.path(), Operation::Bypass, None).unwrap();
    assert_eq!(apo::status_at(root.path()), Status::Bypassed);
    apo::write_operation(root.path(), Operation::Restore, None).unwrap();
    assert_eq!(apo::status_at(root.path()), Status::Enabled);

    let demo = EqConfig::from_profile(Profile::demo());
    let before = fs::read_to_string(&paths.active).unwrap();
    assert!(apo::write_operation(root.path(), Operation::Apply, Some(&demo)).is_err());
    assert_eq!(fs::read_to_string(&paths.active).unwrap(), before, "failed apply leaves the active file alone");
}

#[test]
fn command_line_quoting() {
    use room_tune::platform::quote_arg;
    assert_eq!(quote_arg(r"C:\a b\c.txt"), r#""C:\a b\c.txt""#);
    assert_eq!(quote_arg(r"C:\dir\"), r#""C:\dir\\""#);
    assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
}
