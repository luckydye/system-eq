//! Diagnostic: prints the default devices and the loopback peak level of every output for 3 s.
//! `cargo run --release --example levels [-- --tone <output name part>]` also plays a quiet
//! 440 Hz tone (-36 dBFS) on that output, to check whether the audio engine passes sound.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use room_tune::audio::{self, Tap};
use std::{thread, time::Duration};

fn main() -> anyhow::Result<()> {
    let devices = audio::devices()?;
    let tone_target = std::env::args().skip_while(|a| a != "--tone").nth(1);
    let _tone = match &tone_target {
        Some(part) => {
            let info = devices.outputs.iter().find(|d| d.name.contains(part.as_str())).ok_or_else(|| anyhow::anyhow!("no output matches {part}"))?;
            let host = cpal::host_from_id(cpal::HostId::Wasapi)?;
            let device = host.device_by_id(&cpal::DeviceId(cpal::HostId::Wasapi, info.id.clone())).ok_or_else(|| anyhow::anyhow!("device gone"))?;
            let config = device.default_output_config()?;
            anyhow::ensure!(config.sample_format() == cpal::SampleFormat::F32, "tone needs an f32 device");
            let (rate, channels) = (config.sample_rate() as f32, config.channels() as usize);
            let mut n = 0u64;
            let stream = device.build_output_stream::<f32, _, _>(&config.config(), move |data, _| {
                for frame in data.chunks_exact_mut(channels) {
                    let t = n as f32 / rate; n += 1;
                    let v = if t < 1.5 { 0.0158 * (std::f32::consts::TAU * 440.0 * t).sin() } else { 0.0 };
                    frame.fill(v);
                }
            }, |e| eprintln!("tone stream error: {e}"), None)?;
            stream.play()?;
            println!("Playing test tone on {}", info.name);
            Some(stream)
        }
        None => None,
    };
    let monitors: Vec<_> = devices.outputs.iter().map(|d| (d, audio::monitor(d.id.clone(), Tap::Output))).collect();
    let mut peaks = vec![[0.0_f32; 2]; monitors.len()];
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        for ((_, m), p) in monitors.iter().zip(&mut peaks) { for (s, v) in p.iter_mut().enumerate() { *v = v.max(m.levels.take(s)); } }
    }
    for ((d, m), p) in monitors.iter().zip(&peaks) {
        let db = |v: f32| if v > 0.0 { format!("{:6.1}", 20.0 * v.log10()) } else { "  ----".into() };
        println!("{} {:<45} {} ch {} Hz  L {} R {} dBFS{}", if d.is_default { "*" } else { " " }, d.name, d.channels, d.sample_rate,
            db(p[0]), db(p[1]), if m.levels.failed.load(std::sync::atomic::Ordering::Relaxed) { "  (meter failed)" } else { "" });
    }
    Ok(())
}
