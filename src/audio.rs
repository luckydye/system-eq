use crate::{dsp::{self, Measurement, MicCalibration}, profile::{Profile, PROFILE_VERSION, now}};
use anyhow::{Context, Result, bail, ensure};
use cpal::{traits::{DeviceTrait, HostTrait, StreamTrait}, Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig};
use serde::Serialize;
use std::{sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}, mpsc}, thread, time::{Duration, Instant}};

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub is_default: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Devices {
    pub outputs: Vec<DeviceInfo>,
    pub inputs: Vec<DeviceInfo>,
}

fn host() -> Result<cpal::Host> { cpal::host_from_id(cpal::HostId::Wasapi).context("Windows WASAPI audio is unavailable") }

pub fn devices() -> Result<Devices> {
    let host = host()?;
    let out_default = host.default_output_device().and_then(|d| d.id().ok());
    let in_default = host.default_input_device().and_then(|d| d.id().ok());
    let describe = |device: Device, input: bool| -> Option<DeviceInfo> {
        let config = if input { device.default_input_config() } else { device.default_output_config() }.ok()?;
        let id = device.id().ok()?;
        let description = device.description().ok()?;
        // Windows' friendly name ("Line (Yamaha AG06MK2)") distinguishes endpoints; the short name ("Line") often does not.
        let name = description.extended().first().cloned().unwrap_or_else(|| description.name().into());
        Some(DeviceInfo { is_default: Some(&id) == if input { in_default.as_ref() } else { out_default.as_ref() },
            id: id.1, name, channels: config.channels(), sample_rate: config.sample_rate() })
    };
    Ok(Devices {
        outputs: host.output_devices()?.filter_map(|d| describe(d, false)).filter(|d| d.channels >= 2).collect(),
        inputs: host.input_devices()?.filter_map(|d| describe(d, true)).collect(),
    })
}

#[derive(Clone)]
pub struct Request {
    pub output: DeviceInfo,
    pub input: DeviceInfo,
    pub input_channel: usize,
    pub level_db: f64,
    pub mic: MicCalibration,
}

impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.output.channels >= 2, "A stereo output is required");
        ensure!(self.input_channel < self.input.channels as usize, "Select a valid microphone input channel");
        ensure!(self.level_db.is_finite() && (-42.0..=-18.0).contains(&self.level_db), "Sweep level must be between −42 and −18 dBFS");
        for device in [&self.input, &self.output] {
            ensure!((44_100..=192_000).contains(&device.sample_rate), "Set {} to 44.1–192 kHz in Windows", device.name);
        }
        Ok(())
    }
}

pub fn calibrate(request: &Request, cancel: &Arc<AtomicBool>, progress: &dyn Fn(&str, f32)) -> Result<Profile> {
    request.validate()?;
    check_cancel(cancel)?;
    let left = measure(request, 0, cancel, progress)?;
    check_cancel(cancel)?;
    let right = measure(request, 1, cancel, progress)?;
    let mut p = Profile { version: PROFILE_VERSION, output_id: request.output.id.clone(), output_name: request.output.name.clone(),
        microphone_name: format!("{} / input {}", request.input.name, request.input_channel + 1), calibration_name: request.mic.name.clone(),
        created_unix: now(), demo: false, left, right };
    progress("Fitting conservative bass correction", 0.94);
    dsp::fit(&mut p.left); check_cancel(cancel)?;
    dsp::fit(&mut p.right); check_cancel(cancel)?;
    p.validate(false)?; Ok(p)
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "Calibration cancelled. No new correction was applied"); Ok(())
}

struct Capture {
    samples: Mutex<Vec<f64>>,
    count: AtomicUsize,
    dropped: AtomicBool,
    limit: usize,
}

fn input_stream<T>(device: &Device, config: &StreamConfig, channel: usize, captured: Arc<Capture>, errors: mpsc::Sender<String>) -> Result<Stream>
where T: SizedSample, f64: FromSample<T> {
    let channels = config.channels as usize;
    Ok(device.build_input_stream::<T, _, _>(config, move |data, _| {
        // The worker only takes this lock after dropping the stream. Never block the audio callback.
        let Ok(mut samples) = captured.samples.try_lock() else { captured.dropped.store(true, Ordering::Relaxed); return; };
        for frame in data.chunks_exact(channels) {
            if samples.len() >= captured.limit { captured.dropped.store(true, Ordering::Relaxed); break; }
            samples.push(f64::from_sample(frame[channel]));
        }
        captured.count.store(samples.len(), Ordering::Relaxed);
    }, move |e| { let _ = errors.send(format!("Microphone stream: {e}")); }, None)?)
}

pub fn route_frame<T: Sample + FromSample<f64>>(frame: &mut [T], speaker: usize, value: f64) {
    frame.fill(T::EQUILIBRIUM);
    if let Some(sample) = frame.get_mut(speaker) { *sample = T::from_sample(value); }
}

fn output_stream<T>(device: &Device, config: &StreamConfig, speaker: usize, signal: Vec<f64>, played: Arc<AtomicUsize>, cancel: Arc<AtomicBool>, errors: mpsc::Sender<String>) -> Result<Stream>
where T: SizedSample + FromSample<f64> {
    let channels = config.channels as usize; let mut position = 0;
    Ok(device.build_output_stream::<T, _, _>(config, move |data, _| {
        let cancelled = cancel.load(Ordering::Relaxed);
        for frame in data.chunks_exact_mut(channels) {
            let value = if cancelled { 0.0 } else { signal.get(position).copied().unwrap_or(0.0) };
            route_frame(frame, speaker, value);
            position = position.saturating_add(1);
        }
        played.store(position, Ordering::Relaxed);
    }, move |e| { let _ = errors.send(format!("Speaker stream: {e}")); }, None)?)
}

fn start_input(device: &Device, format: &SupportedStreamConfig, channel: usize, captured: Arc<Capture>, errors: mpsc::Sender<String>) -> Result<Stream> {
    let config = format.config();
    match format.sample_format() {
        SampleFormat::F32 => input_stream::<f32>(device, &config, channel, captured, errors),
        SampleFormat::F64 => input_stream::<f64>(device, &config, channel, captured, errors),
        SampleFormat::I16 => input_stream::<i16>(device, &config, channel, captured, errors),
        SampleFormat::I32 => input_stream::<i32>(device, &config, channel, captured, errors),
        SampleFormat::U16 => input_stream::<u16>(device, &config, channel, captured, errors),
        format => bail!("Unsupported microphone format {format}; select a standard PCM format in Windows"),
    }
}

fn start_output(device: &Device, format: &SupportedStreamConfig, speaker: usize, signal: Vec<f64>, played: Arc<AtomicUsize>, cancel: Arc<AtomicBool>, errors: mpsc::Sender<String>) -> Result<Stream> {
    let config = format.config();
    match format.sample_format() {
        SampleFormat::F32 => output_stream::<f32>(device, &config, speaker, signal, played, cancel, errors),
        SampleFormat::F64 => output_stream::<f64>(device, &config, speaker, signal, played, cancel, errors),
        SampleFormat::I16 => output_stream::<i16>(device, &config, speaker, signal, played, cancel, errors),
        SampleFormat::I32 => output_stream::<i32>(device, &config, speaker, signal, played, cancel, errors),
        SampleFormat::U16 => output_stream::<u16>(device, &config, speaker, signal, played, cancel, errors),
        format => bail!("Unsupported speaker format {format}; select a standard PCM format in Windows"),
    }
}

/// Live peak levels (linear, two slots), written by an audio callback and drained by the UI.
#[derive(Default)]
pub struct Levels { peaks: [AtomicU32; 2], pub failed: AtomicBool }

impl Levels {
    /// Returns the highest peak since the last call and resets it.
    pub fn take(&self, slot: usize) -> f32 { f32::from_bits(self.peaks[slot].swap(0, Ordering::Relaxed)) }

    fn feed(&self, peaks: [f32; 2]) {
        // Non-negative IEEE floats order like their bit patterns, so fetch_max works on the bits.
        for (slot, peak) in self.peaks.iter().zip(peaks) { slot.fetch_max(peak.to_bits(), Ordering::Relaxed); }
    }
}

/// What a level monitor listens to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tap {
    /// Everything playing on an output (WASAPI loopback), left and right.
    Output,
    /// One microphone input channel (reported in slot 0).
    Input(usize),
}

/// A running level meter. The stream lives on its own thread and stops when this is dropped.
pub struct Monitor { stop: Arc<AtomicBool>, pub levels: Arc<Levels> }

impl Drop for Monitor { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); } }

fn level_stream<T>(device: &Device, config: &StreamConfig, picks: [Option<usize>; 2], levels: Arc<Levels>) -> Result<Stream>
where T: SizedSample, f32: FromSample<T> {
    let channels = config.channels as usize;
    let errors = levels.clone();
    Ok(device.build_input_stream::<T, _, _>(config, move |data, _| {
        let mut peaks = [0.0_f32; 2];
        for frame in data.chunks_exact(channels) {
            for (peak, pick) in peaks.iter_mut().zip(picks) {
                if let Some(c) = pick { *peak = peak.max(f32::from_sample(frame[c]).abs()); }
            }
        }
        levels.feed(peaks);
    }, move |_| errors.failed.store(true, Ordering::Relaxed), None)?)
}

fn run_monitor(device_id: &str, tap: Tap, levels: Arc<Levels>, stop: &AtomicBool) -> Result<()> {
    let device = host()?.device_by_id(&cpal::DeviceId(cpal::HostId::Wasapi, device_id.into())).context("Device disconnected")?;
    // Building an input stream on an output device makes cpal capture it in loopback mode.
    let format = if tap == Tap::Output { device.default_output_config()? } else { device.default_input_config()? };
    let channels = format.channels() as usize;
    let picks = match tap {
        Tap::Output => [Some(0), (channels > 1).then_some(1)],
        Tap::Input(c) => { ensure!(c < channels, "Invalid input channel"); [Some(c), None] }
    };
    let config = format.config();
    let stream = match format.sample_format() {
        SampleFormat::F32 => level_stream::<f32>(&device, &config, picks, levels),
        SampleFormat::F64 => level_stream::<f64>(&device, &config, picks, levels),
        SampleFormat::I16 => level_stream::<i16>(&device, &config, picks, levels),
        SampleFormat::I32 => level_stream::<i32>(&device, &config, picks, levels),
        SampleFormat::U16 => level_stream::<u16>(&device, &config, picks, levels),
        format => bail!("Unsupported format {format}"),
    }?;
    stream.play()?;
    while !stop.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(50)); }
    Ok(())
}

/// Starts a level meter on a device. Failures are reported through `Levels::failed`.
pub fn monitor(device_id: String, tap: Tap) -> Monitor {
    let monitor = Monitor { stop: Arc::new(AtomicBool::new(false)), levels: Arc::new(Levels::default()) };
    let (stop, levels) = (monitor.stop.clone(), monitor.levels.clone());
    thread::spawn(move || {
        if run_monitor(&device_id, tap, levels.clone(), &stop).is_err() { levels.failed.store(true, Ordering::Relaxed); }
    });
    monitor
}

fn measure(request: &Request, speaker: usize, cancel: &Arc<AtomicBool>, progress: &dyn Fn(&str, f32)) -> Result<Measurement> {
    let host = host()?;
    let input = host.device_by_id(&cpal::DeviceId(cpal::HostId::Wasapi, request.input.id.clone())).context("Microphone disconnected")?;
    let output = host.device_by_id(&cpal::DeviceId(cpal::HostId::Wasapi, request.output.id.clone())).context("Speaker output disconnected")?;
    let in_format = input.default_input_config()?; let out_format = output.default_output_config()?;
    ensure!(in_format.channels() as usize > request.input_channel && out_format.channels() >= 2, "Channel configuration changed. Refresh audio devices");
    let in_rate = in_format.sample_rate(); let out_rate = out_format.sample_rate();
    ensure!((44_100..=192_000).contains(&in_rate) && (44_100..=192_000).contains(&out_rate), "Set both devices to 44.1–192 kHz and refresh");
    let limit = in_rate as usize * 20;
    let captured = Arc::new(Capture { samples: Mutex::new(Vec::with_capacity(limit)), count: AtomicUsize::new(0), dropped: AtomicBool::new(false), limit });
    let played = Arc::new(AtomicUsize::new(0));
    let (error_tx, error_rx) = mpsc::channel();
    let capture = start_input(&input, &in_format, request.input_channel, captured.clone(), error_tx.clone())?;
    let signal = dsp::sweep(out_rate, request.level_db);
    let signal_len = signal.len();
    let player = start_output(&output, &out_format, speaker, signal, played.clone(), cancel.clone(), error_tx)?;
    let side = if speaker == 0 { "LEFT" } else { "RIGHT" };
    progress(&format!("{side} speaker · measuring background noise"), speaker as f32 * 0.46);
    capture.play()?;
    let started = Instant::now();
    let check = || -> Result<()> {
        check_cancel(cancel)?;
        if let Ok(error) = error_rx.try_recv() { bail!(error); }
        ensure!(!captured.dropped.load(Ordering::Relaxed), "Microphone samples were dropped; close busy applications and try again");
        Ok(())
    };
    while captured.count.load(Ordering::Relaxed) < in_rate as usize {
        check()?;
        ensure!(started.elapsed() < Duration::from_secs(5), "No microphone samples received. Check device access and connection");
        thread::sleep(Duration::from_millis(20));
    }
    check()?; player.play()?;
    let play_started = Instant::now();
    while played.load(Ordering::Relaxed) < signal_len {
        check()?;
        ensure!(play_started.elapsed() < Duration::from_secs(16), "Playback timed out. Check the selected speaker output");
        progress(&format!("Measuring {side} · keep the room quiet"), speaker as f32 * 0.46 + 0.03 + 0.36 * (played.load(Ordering::Relaxed) as f32 / signal_len as f32).min(1.0));
        thread::sleep(Duration::from_millis(100));
    }
    let tail = Instant::now();
    while tail.elapsed() < Duration::from_millis(1500) { check()?; thread::sleep(Duration::from_millis(20)); }
    check()?;
    drop(player); drop(capture);
    let recording = std::mem::take(&mut *captured.samples.lock().map_err(|_| anyhow::anyhow!("Capture buffer failed"))?);
    ensure!(recording.len() >= (in_rate as f64 * 12.0) as usize, "The microphone stopped supplying samples during measurement");
    let noise = &recording[..in_rate as usize * 3 / 4];
    progress(&format!("Analyzing {side} speaker"), speaker as f32 * 0.46 + 0.43);
    // Generate the reference at the capture rate. Different input/output rates need no lossy resampling.
    let reference = dsp::sweep(in_rate, request.level_db);
    dsp::analyze(&recording, &reference, noise, in_rate, &request.mic)
}
