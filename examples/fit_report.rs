//! Diagnostic: fits a correction to a measurement and prints the result (writes nothing).
//! `cargo run --release --example fit_report -- <file.roomtune | file.swproj>`
use room_tune::{dsp::{self, Measurement}, profile::Profile, soundid};
use std::path::Path;

fn report(name: &str, m: &mut Measurement) {
    let started = std::time::Instant::now();
    dsp::fit(m);
    let elapsed = started.elapsed();
    let smoothed = m.smoothed();
    let (low, high) = m.range.unwrap();
    let error = |with_eq: bool| {
        let v: Vec<f64> = m.frequencies.iter().zip(&smoothed).filter(|(f, _)| (low..=high).contains(*f))
            .map(|(&f, &r)| r + if with_eq { m.correction(f) } else { 0.0 }).collect();
        (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt()
    };
    println!("{name}: range {low:.0}-{high:.0} Hz, {} filters, RMS deviation {:.2} dB -> {:.2} dB ({elapsed:.0?})",
        m.filters.len(), error(false), error(true));
    for f in &m.filters { println!("    {:>8.1} Hz  {:+5.1} dB  Q {:.2}", f.frequency, f.gain, f.q); }
    let at = [30.0, 50.0, 80.0, 100.0, 125.0, 200.0, 315.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 12000.0];
    let pick = |f: f64| (0..m.frequencies.len()).min_by(|&a, &b| (m.frequencies[a] / f).ln().abs().total_cmp(&(m.frequencies[b] / f).ln().abs())).unwrap();
    println!("    smoothed: {}", at.iter().map(|&f| format!("{f}:{:+.1}", smoothed[pick(f)])).collect::<Vec<_>>().join(" "));
    println!("    with EQ:  {}", at.iter().map(|&f| format!("{f}:{:+.1}", smoothed[pick(f)] + m.correction(m.frequencies[pick(f)]))).collect::<Vec<_>>().join(" "));
}

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("pass a .roomtune or .swproj file");
    let (mut l, mut r) = if path.ends_with(".swproj") { soundid::load(Path::new(&path))? } else {
        let p: Profile = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        (p.left, p.right)
    };
    report("Left", &mut l);
    report("Right", &mut r);
    Ok(())
}
