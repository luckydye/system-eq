# RoomTune — room correction & EQ for Windows

RoomTune is one app for one-time microphone room calibration and manual EQ. It applies the result to **all Windows playback**, even after you close it.

It is a single native executable (`room-tune.exe`, about 7 MB, Rust + egui). It has no .NET, Electron, browser, account, or background service of its own.

## How playback works

Windows only allows system-wide EQ inside its audio engine, as an audio processing object (APO). RoomTune uses the open-source [Equalizer APO](https://sourceforge.net/projects/equalizerapo/) engine and **sets it up for you**. On the setup screen, one button does all of this, with a single Windows permission prompt:

1. Downloads Equalizer APO 1.4.2 (12 MB) and checks it against a pinned SHA-256, which matches the Chocolatey package checksum.
2. Copies it to an admin-only folder, checks it again, and installs it silently. The installer opens Equalizer APO's Device Selector; RoomTune closes it immediately.
3. Enables the engine on your speakers with the same registry changes the Device Selector makes, so Equalizer APO's uninstaller still undoes them.
4. Restarts Windows audio. If that fails, RoomTune asks you to restart Windows instead.

Equalizer APO's installer gives users write access to its config folder. After setup, EQ edits are written directly and heard within about a quarter second, with no further permission prompts.

## Use

- **Setup screen (first launch):** pick your speakers and measurement microphone; each has a live level meter. Then install or enable the playback engine. You can come back later via ••• › Devices & engine.
- **Graph:** shows both speakers at once, left solid and right dashed. You see the measured response, the response with EQ, and the EQ curve, with a live output meter beside it.
- **EQ switch** (top bar): turns the EQ on or off, so you can compare by ear.
- **Calibrate room…:** opens a dialog with the mic, input channel, input meter, sweep level, and an optional mic calibration file. Each speaker gets a noise baseline and a 10 s sweep. RoomTune then fits cut-only filters between 30 and 300 Hz, per speaker. If the EQ is on, it is bypassed during the measurement and the new result is applied afterwards.
- **Room correction chip:** click to switch room correction on or off. Hover to see the fitted filters and why a band wasn't cut, such as low SNR or too little bass to correct.
- **Manual EQ:** one shared EQ for both speakers, with up to 16 peak, low-shelf, or high-shelf bands.
  - Add bands with **+ Band** or by double-clicking the graph.
  - Drag a point to change frequency and gain; scroll over it to change Q.
  - Edit the selected band in the strip below the graph. Delete removes it.
- **••• menu:** Open or Save a `.roomtune` file (room correction plus manual EQ), Export for Equalizer APO (plain filter text), and the demo.

RoomTune adds a shared preamp of 1 dB plus the largest boost, so EQ boosts can't clip.

## Files

- `%LOCALAPPDATA%\RoomTune\`:
  - `eq.roomtune`: your current EQ
  - `last-measurement.roomtune`: the latest room measurement
  - `settings.json`: chosen devices
  - `downloads\`: the verified engine installer

Raw recordings stay in memory only.

In Equalizer APO's config folder (`C:\Program Files\EqualizerAPO\config`):

```
config.txt                     Your config plus one marked include block (added once)
RoomTune\active.txt            Current EQ, or a bypass comment
RoomTune\last-enabled.txt      Last enabled EQ
RoomTune\config-backup-*.txt   Copy of config.txt from before RoomTune changed it
```

**To remove everything:** uninstall *Equalizer APO* in Windows Settings › Apps; this also restores your speakers' original audio effects. To remove only RoomTune's EQ, delete the lines between the `ROOMTUNE MANAGED BLOCK` markers in `config.txt`.

## Limits

- Room correction covers bass resonances only, measured from a single listening position. Manual EQ covers the full range.
- The "with EQ" curve is a prediction, not a second measurement.
- **ASIO and WASAPI exclusive-mode playback bypass the engine**, for example DAWs using native drivers and some games.
- Automatic engine setup supports x64 Windows only. One EQ applies to one output at a time.
- Some drivers need a full Windows restart before the engine runs.

## Build and test

Requires Rust 1.88+ (edition 2024) on Windows.

```powershell
cargo build --release          # target\release\room-tune.exe
cargo test --release           # no audio, downloads, or system changes
cargo run --release --example levels -- --tone "Analogue 1/2"   # diagnose: is audio reaching an output?
```

The tests cover:
- measurement maths and room-correction fitting;
- filters matching Equalizer APO's biquads;
- exported frequencies surviving Equalizer APO's parser;
- EQ export, headroom, and limits;
- `.roomtune` loading;
- config preservation and live re-apply against a fake engine folder;
- engine install-mode selection.

`room-tune.exe --demo` starts with demo data. `--helper …` is the internal elevated helper.

## Layout

| Path | Role |
|---|---|
| `src/main.rs` | egui UI: setup screen, graph, band editor, meters, live apply |
| `src/eq.rs` | EQ model: bands, shelves, room + manual combination, headroom |
| `src/engine.rs` | Engine download/verify, silent install, endpoint registration |
| `src/apo.rs` | Engine config files, managed block, elevated helper |
| `src/audio.rs` | WASAPI device listing, sweep playback, capture, level meters |
| `src/dsp.rs` | Sweep, FFT analysis, room-correction fitting |
| `src/profile.rs` | Room measurement profiles, atomic writes |
| `src/platform.rs` | UAC, registry & permissions, WinHTTP download, processes |
| `examples/levels.rs` | Diagnostic: output levels and test tone |
| `tests/core.rs` | Integration tests |

RoomTune is MIT licensed (see `LICENSE.txt`). Equalizer APO is GPLv2. RoomTune downloads the unmodified official installer and interacts with it only through registry entries and its [documented config files](https://sourceforge.net/p/equalizerapo/wiki/Configuration%20reference/).
