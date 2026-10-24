//! Echo test for the PC app's microphone. Plays a voice clip through the
//! speakers and records the microphone two ways at once: the way the app does
//! now (cpal, raw), and as a Windows "communications" stream, which turns on
//! Windows' own echo cancellation. Then it reports how loud her echo is next
//! to your voice in each recording.
//!
//!   cargo run --release -p adrian-desktop --example aec_probe -- <voice.wav> <out dir>
//!
//! Phases: 3 s quiet, 10 s her voice alone, 10 s her voice while you talk over
//! it, 5 s you talking alone. The WAVs are saved so they can be listened to.

#[cfg(windows)]
mod wasapi {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    use windows::core::Interface;

    /// Captures the default microphone as a communications stream (mono f32 at 48 kHz).
    pub fn capture(stop: Arc<AtomicBool>, out: Arc<Mutex<Vec<f32>>>) -> windows::core::Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eCapture, eConsole)?;
            let client: IAudioClient2 = dev.Activate(CLSCTX_ALL, None)?;
            let props = AudioClientProperties {
                cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: AudioCategory_Communications,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            };
            client.SetClientProperties(&props)?;
            let fmt = WAVEFORMATEX {
                wFormatTag: 3, // IEEE float
                nChannels: 1,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 4,
                nBlockAlign: 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                1_000_000,
                0,
                &fmt,
                None,
            )?;
            // Don't let Windows turn other sounds down because a "call" started.
            if let Ok(ctl2) = client
                .GetService::<IAudioSessionControl>()
                .and_then(|c| c.cast::<IAudioSessionControl2>())
            {
                let _ = ctl2.SetDuckingPreference(true);
            }
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            while !stop.load(Ordering::Relaxed) {
                loop {
                    let n = cap.GetNextPacketSize()?;
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let mut o = out.lock().unwrap();
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                        o.extend(std::iter::repeat_n(0.0, frames as usize));
                    } else {
                        o.extend_from_slice(std::slice::from_raw_parts(
                            data as *const f32,
                            frames as usize,
                        ));
                    }
                    drop(o);
                    cap.ReleaseBuffer(frames)?;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            client.Stop()?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn main() {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let args: Vec<String> = std::env::args().collect();
    let (voice_path, out_dir) = (
        args.get(1).expect("voice.wav"),
        args.get(2).expect("out dir"),
    );
    std::fs::create_dir_all(out_dir).unwrap();
    let bytes = std::fs::read(voice_path).unwrap();
    let voice_rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let voice: Vec<f32> = bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect();

    let host = cpal::default_host();
    let input = host.default_input_device().expect("mic");
    let output = host.default_output_device().expect("speakers");
    println!("Mic: {}", input.name().unwrap_or_default());
    println!("Speakers: {}", output.name().unwrap_or_default());

    // raw capture, as the app does it
    let in_cfg = input.default_input_config().unwrap();
    let raw_rate = in_cfg.sample_rate().0;
    let in_ch = in_cfg.channels() as usize;
    let raw = Arc::new(Mutex::new(Vec::<f32>::new()));
    let raw2 = raw.clone();
    let in_stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => input.build_input_stream(
            &in_cfg.into(),
            move |d: &[f32], _| {
                raw2.lock().unwrap().extend(
                    d.chunks(in_ch)
                        .map(|f| f.iter().sum::<f32>() / in_ch as f32),
                )
            },
            |e| eprintln!("mic: {e}"),
            None,
        ),
        cpal::SampleFormat::I16 => input.build_input_stream(
            &in_cfg.into(),
            move |d: &[i16], _| {
                raw2.lock().unwrap().extend(
                    d.chunks(in_ch)
                        .map(|f| f.iter().map(|&s| s as f32 / 32768.0).sum::<f32>() / in_ch as f32),
                )
            },
            |e| eprintln!("mic: {e}"),
            None,
        ),
        f => panic!("mic format {f}"),
    }
    .unwrap();

    // communications capture
    let stop = Arc::new(AtomicBool::new(false));
    let comms = Arc::new(Mutex::new(Vec::<f32>::new()));
    let (s2, c2) = (stop.clone(), comms.clone());
    let comms_thread = std::thread::spawn(move || wasapi::capture(s2, c2));

    // playback: her voice, resampled by nearest sample to the speaker rate
    let out_cfg = output.default_output_config().unwrap();
    let out_rate = out_cfg.sample_rate().0;
    let out_ch = out_cfg.channels() as usize;
    let playing = Arc::new(AtomicBool::new(false));
    let pos = Arc::new(AtomicUsize::new(0));
    let (p2, pos2, v2) = (playing.clone(), pos.clone(), Arc::new(voice));
    let step = voice_rate as f64 / out_rate as f64;
    assert!(
        out_cfg.sample_format() == cpal::SampleFormat::F32,
        "speaker format"
    );
    let out_stream = output
        .build_output_stream(
            &out_cfg.into(),
            move |d: &mut [f32], _| {
                for f in d.chunks_mut(out_ch) {
                    let mut s = 0.0;
                    if p2.load(Ordering::Relaxed) {
                        let i = pos2.fetch_add(1, Ordering::Relaxed);
                        s = v2[((i as f64 * step) as usize) % v2.len()] * 0.8;
                    }
                    f.iter_mut().for_each(|x| *x = s);
                }
            },
            |e| eprintln!("speakers: {e}"),
            None,
        )
        .unwrap();

    in_stream.play().unwrap();
    out_stream.play().unwrap();
    let t0 = Instant::now();
    let mut marks = vec![];
    let mut phase = |label: &str, secs: u64, play: bool| {
        println!("{label}");
        playing.store(play, Ordering::Relaxed);
        marks.push(t0.elapsed().as_secs_f64());
        std::thread::sleep(Duration::from_secs(secs));
    };
    std::thread::sleep(Duration::from_millis(500));
    phase("1/4  Stay quiet (3 s)...", 3, false);
    phase("2/4  Her voice plays. Stay quiet (10 s)...", 10, true);
    phase(
        "3/4  Her voice plays. TALK over her normally, like you're interrupting (10 s)...",
        10,
        true,
    );
    phase(
        "4/4  She stops. Keep talking the same way (5 s)...",
        5,
        false,
    );
    marks.push(t0.elapsed().as_secs_f64());
    stop.store(true, Ordering::Relaxed);
    drop(in_stream);
    drop(out_stream);
    let comms_ok = comms_thread.join().unwrap();

    let save = |name: &str, s: &[f32], rate: u32| {
        let mut b = Vec::with_capacity(44 + s.len() * 2);
        b.extend(b"RIFF");
        b.extend((36 + s.len() as u32 * 2).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend((rate * 2).to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend((s.len() as u32 * 2).to_le_bytes());
        for &x in s {
            b.extend(((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(std::path::Path::new(out_dir).join(name), b).unwrap();
    };
    // Both recordings started within a few ms of t0 + 0.5 s; skip 0.5 s at each phase edge.
    let rms = |s: &[f32], rate: u32, a: f64, b: f64| {
        let (i, j) = (
            ((a + 0.5) * rate as f64) as usize,
            ((b - 0.2) * rate as f64) as usize,
        );
        let seg = &s[i.min(s.len())..j.min(s.len())];
        (seg.iter().map(|x| x * x).sum::<f32>() / seg.len().max(1) as f32).sqrt()
    };
    let report = |name: &str, s: &[f32], rate: u32| {
        save(&format!("{name}.wav"), s, rate);
        let r: Vec<f32> = (0..4)
            .map(|k| rms(s, rate, marks[k] - marks[0], marks[k + 1] - marks[0]))
            .collect();
        let db = |a: f32, b: f32| 20.0 * (a.max(1e-6) / b.max(1e-6)).log10();
        println!(
            "{name:>6}: room {:.4}  echo {:.4}  you+echo {:.4}  you {:.4} | echo vs your voice {:+.1} dB | you over her {:+.1} dB",
            r[0],
            r[1],
            r[2],
            r[3],
            db(r[1], r[3]),
            db(r[2], r[1])
        );
    };
    println!();
    report("raw", &raw.lock().unwrap(), raw_rate);
    match comms_ok {
        Ok(()) => report("comms", &comms.lock().unwrap(), 48_000),
        Err(e) => println!(" comms: couldn't open a communications stream: {e}"),
    }
    println!(
        "\nLower 'echo vs your voice' and higher 'you over her' are better. WAVs in {out_dir}"
    );
}

#[cfg(not(windows))]
fn main() {}
