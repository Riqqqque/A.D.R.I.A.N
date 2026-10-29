//! Microphone in, her voice out.
//!
//! The mic is always on, but audio only leaves the PC while someone is
//! talking: a local loudness gate (with a little audio from just before it
//! opened) decides that, and the server finds the exact words and the wake
//! phrase. While she is talking the gate stays shut (no echo cancellation
//! yet), except with push-to-talk.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// Audio to and from the server: 48 kHz mono.
pub const RATE: u32 = 48_000;
/// 20 ms frames.
const FRAME: usize = 960;
/// Audio kept from before the gate opens (300 ms), so first syllables aren't lost.
const PREROLL_FRAMES: usize = 15;
/// Frames above the threshold before it counts as talking (60 ms).
const OPEN_FRAMES: u32 = 3;
/// Quiet frames before talking counts as over (1.2 s: people pause mid-sentence).
const CLOSE_FRAMES: u32 = 60;
/// Longest single stretch sent (the server cuts long speech anyway).
const MAX_FRAMES: u32 = 30 * 50;
/// Her voice counts as playing this long after it stops (room echo).
const ECHO_TAIL_MS: u64 = 350;
/// Talking over her has to last this long (80 ms) before it counts.
const BARGE_FRAMES: u32 = 4;
/// How much louder than her echo you have to be to cut in.
const BARGE_OVER_ECHO: f32 = 3.0;
/// Her voice while you cut in: dropped to this, right away, on the PC.
const DUCK: f32 = 0.3;
/// Most audio held for playback before old audio is dropped (seconds).
const MAX_BUFFER_SECS: usize = 8;

/// To the device thread: a stream failed, she has something to say, or
/// Windows' default speakers or mic changed.
enum Ctl {
    Failed(String),
    Wake,
    DefaultChanged,
}

/// The speakers are let go after this much silence, so the app isn't
/// feeding the audio engine silence all day.
const SPEAKER_IDLE: Duration = Duration::from_secs(3);

pub enum Up {
    Start,
    Pcm(Vec<u8>),
    End,
}

pub struct Shared {
    out: Mutex<VecDeque<f32>>,
    out_rate: AtomicU32,
    down: Mutex<Resampler>,
    /// ms since `epoch` when her audio was last audible.
    last_out_ms: AtomicU64,
    epoch: Instant,
    pub out_level: AtomicU32,
    /// Raw loudness (RMS, f32 bits) of what the speakers played last.
    out_rms: AtomicU32,
    /// Someone is talking over her: play her quieter until the server stops her.
    duck: AtomicBool,
    pub mic_level: AtomicU32,
    pub talking: AtomicBool,
    pub ptt: AtomicBool,
    /// Manual mic threshold (f32 bits); 0 = automatic.
    pub threshold: AtomicU32,
    pub mic_error: Mutex<Option<String>>,
    /// Wakes the device thread to start the speakers.
    wake: Mutex<Option<mpsc::Sender<Ctl>>>,
    /// The mic and speakers in use (names), for the settings page.
    pub devices: Mutex<(String, String)>,
}

impl Shared {
    pub fn new(threshold: f32) -> Arc<Self> {
        Arc::new(Shared {
            out: Mutex::new(VecDeque::new()),
            out_rate: AtomicU32::new(RATE),
            down: Mutex::new(Resampler::new(RATE, RATE)),
            last_out_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            out_level: AtomicU32::new(0),
            out_rms: AtomicU32::new(0),
            duck: AtomicBool::new(false),
            mic_level: AtomicU32::new(0),
            talking: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            threshold: AtomicU32::new(threshold.to_bits()),
            mic_error: Mutex::new(None),
            wake: Mutex::new(None),
            devices: Mutex::new((String::new(), String::new())),
        })
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Her voice is playing, or just stopped.
    pub fn playing(&self) -> bool {
        let last = self.last_out_ms.load(Ordering::Relaxed);
        last != 0 && self.now_ms().saturating_sub(last) < ECHO_TAIL_MS
            || !self.out.lock().map(|q| q.is_empty()).unwrap_or(true)
    }

    /// Reply audio from the server (48 kHz mono s16le).
    pub fn push_reply(&self, bytes: &[u8]) {
        let input: Vec<f32> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let mut converted = Vec::with_capacity(input.len() * 2);
        if let Ok(mut r) = self.down.lock() {
            r.push(&input, &mut converted);
        }
        if let Ok(mut q) = self.out.lock() {
            q.extend(converted);
            let cap = self.out_rate.load(Ordering::Relaxed) as usize * MAX_BUFFER_SECS;
            while q.len() > cap {
                q.pop_front();
            }
        }
        if let Some(tx) = self.wake.lock().ok().and_then(|w| w.clone()) {
            let _ = tx.send(Ctl::Wake);
        }
    }

    /// She was interrupted: drop what's still queued.
    pub fn stop_reply(&self) {
        if let Ok(mut q) = self.out.lock() {
            q.clear();
        }
    }
}

/// Streaming linear resampler, good enough for speech.
pub struct Resampler {
    step: f64,
    pos: f64,
    hist: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler {
            step: from as f64 / to as f64,
            pos: 0.0,
            hist: Vec::new(),
        }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        self.hist.extend_from_slice(input);
        while self.pos + 1.0 < self.hist.len() as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            out.push(self.hist[i] + (self.hist[i + 1] - self.hist[i]) * f);
            self.pos += self.step;
        }
        let used = (self.pos as usize).min(self.hist.len());
        self.hist.drain(..used);
        self.pos -= used as f64;
    }
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn level(shared: &AtomicU32, rms: f32) {
    // A 0..1 level for the orb: speech sits around 0.02-0.15 RMS.
    let v = (rms * 7.0).min(1.0);
    let prev = f32::from_bits(shared.load(Ordering::Relaxed));
    let next = if v > prev { v } else { prev * 0.85 + v * 0.15 };
    shared.store(next.to_bits(), Ordering::Relaxed);
}

/// Decides when someone is talking, frame by frame.
pub struct Gate {
    floor: f32,
    open: bool,
    above: u32,
    quiet: u32,
    frames: u32,
    preroll: VecDeque<Vec<f32>>,
}

pub enum GateEvent {
    Open(Vec<Vec<f32>>),
    Frame(Vec<f32>),
    Close,
    Nothing,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Gate {
            floor: 0.003,
            open: false,
            above: 0,
            quiet: 0,
            frames: 0,
            preroll: VecDeque::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// One 20 ms frame. `force`: Some(true) holds it open (push-to-talk),
    /// Some(false) holds it shut, None decides by loudness. `echo`: how loud
    /// her own voice is expected to be in the mic right now (0 when she's
    /// quiet); talking over her has to be clearly louder than that.
    pub fn frame(&mut self, f: Vec<f32>, manual: f32, force: Option<bool>, echo: f32) -> GateEvent {
        let r = rms(&f);
        // The background level: it follows quiet moments quickly and loud
        // ones only very slowly, so speech (hers through the speakers, or a
        // show) doesn't teach it that the room is loud. Never while she talks.
        if !self.open && force.is_none() && echo == 0.0 {
            let k = if r < self.floor { 0.1 } else { 0.002 };
            self.floor = (self.floor * (1.0 - k) + r * k).clamp(0.0005, 0.02);
        }
        let thr = if manual > 0.0 {
            manual
        } else {
            // Normal speech at a desk mic is 0.02-0.1; never ask for more than that.
            (self.floor * 3.0).clamp(0.012, 0.035)
        };
        // Over her voice: louder than her echo, for a little longer.
        let (thr, need) = if echo > 0.0 {
            (thr.max(echo * BARGE_OVER_ECHO + 0.01), BARGE_FRAMES)
        } else {
            (thr, OPEN_FRAMES)
        };
        if self.open {
            self.frames += 1;
            let quiet = r < (thr * 0.7).max(echo * 1.5) && force != Some(true);
            self.quiet = if quiet { self.quiet + 1 } else { 0 };
            if force == Some(false) || self.quiet >= CLOSE_FRAMES || self.frames >= MAX_FRAMES {
                self.open = false;
                self.above = 0;
                self.quiet = 0;
                self.preroll.clear();
                return GateEvent::Close;
            }
            return GateEvent::Frame(f);
        }
        self.preroll.push_back(f);
        while self.preroll.len() > PREROLL_FRAMES {
            self.preroll.pop_front();
        }
        if force == Some(false) {
            self.above = 0;
            return GateEvent::Nothing;
        }
        self.above = if r > thr { self.above + 1 } else { 0 };
        if self.above >= need || force == Some(true) {
            self.open = true;
            self.frames = 0;
            self.quiet = 0;
            return GateEvent::Open(self.preroll.drain(..).collect());
        }
        GateEvent::Nothing
    }
}

fn to_bytes(f: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(f.len() * 2);
    for v in f {
        b.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

/// Start the mic and the speakers on their own threads; they rebuild
/// themselves after a device error (headset unplugged, default changed).
pub fn start(shared: Arc<Shared>, up: UnboundedSender<Up>) {
    let (mic_tx, mic_rx) = mpsc::channel::<(u32, Vec<f32>)>();
    // Gate and framing.
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("adrian-gate".into())
            .spawn(move || {
                let mut gate = Gate::new();
                // How much of her voice the mic picks up (mic RMS / speaker RMS),
                // learned while she talks; starts high so early on only clear
                // talking over her counts.
                let mut coupling: f32 = 1.0;
                let mut recent_out: VecDeque<f32> = VecDeque::new();
                let mut rs: Option<(u32, Resampler)> = None;
                let mut buf: Vec<f32> = Vec::with_capacity(FRAME * 2);
                let mut converted = Vec::new();
                while let Ok((rate, samples)) = mic_rx.recv() {
                    if rs.as_ref().map(|(r, _)| *r) != Some(rate) {
                        rs = Some((rate, Resampler::new(rate, RATE)));
                    }
                    converted.clear();
                    rs.as_mut().unwrap().1.push(&samples, &mut converted);
                    buf.extend_from_slice(&converted);
                    while buf.len() >= FRAME {
                        let f: Vec<f32> = buf.drain(..FRAME).collect();
                        let r = rms(&f);
                        level(&shared.mic_level, r);
                        // Her voice reaches the mic a little late: compare with the last 240 ms.
                        recent_out
                            .push_back(f32::from_bits(shared.out_rms.load(Ordering::Relaxed)));
                        while recent_out.len() > 12 {
                            recent_out.pop_front();
                        }
                        let out_ref = recent_out.iter().copied().fold(0.0, f32::max);
                        let playing = shared.playing();
                        if playing && out_ref > 0.004 && !gate.is_open() {
                            let ratio = r / out_ref;
                            let k = if ratio < coupling { 0.05 } else { 0.01 };
                            coupling = (coupling + (ratio - coupling) * k).clamp(0.02, 2.0);
                        }
                        let echo = if playing {
                            (coupling * out_ref).max(0.001)
                        } else {
                            0.0
                        };
                        let force = if shared.ptt.load(Ordering::Relaxed) {
                            Some(true)
                        } else {
                            None
                        };
                        let manual = f32::from_bits(shared.threshold.load(Ordering::Relaxed));
                        match gate.frame(f, manual, force, echo) {
                            GateEvent::Open(pre) => {
                                let _ = up.send(Up::Start);
                                for p in pre {
                                    let _ = up.send(Up::Pcm(to_bytes(&p)));
                                }
                            }
                            GateEvent::Frame(f) => {
                                let _ = up.send(Up::Pcm(to_bytes(&f)));
                            }
                            GateEvent::Close => {
                                let _ = up.send(Up::End);
                            }
                            GateEvent::Nothing => {}
                        }
                        shared.talking.store(gate.is_open(), Ordering::Relaxed);
                        // Cutting in: drop her voice now; the server stops her a moment later.
                        shared
                            .duck
                            .store(gate.is_open() && playing, Ordering::Relaxed);
                    }
                }
            })
            .expect("gate thread");
    }
    // Follow Windows' default speakers and mic as they change.
    watch::start(shared.clone());
    // Devices. The mic always runs; the speakers only while she has audio.
    std::thread::Builder::new()
        .name("adrian-audio".into())
        .spawn(move || {
            let mut retry = Duration::from_millis(300);
            loop {
                let (tx, rx) = mpsc::channel::<Ctl>();
                *shared.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
                let failure = match open_streams(&shared, mic_tx.clone(), tx) {
                    Ok((_mic, speakers)) => {
                        *shared.mic_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        retry = Duration::from_millis(300);
                        let mut on = false;
                        let mut last_sound = Instant::now();
                        // Something queued before the streams existed (or before a switch).
                        let pending = !shared.out.lock().map(|q| q.is_empty()).unwrap_or(true);
                        if pending && speakers.play().is_ok() {
                            on = true;
                        }
                        loop {
                            match rx.recv_timeout(Duration::from_secs(1)) {
                                Ok(Ctl::Failed(e)) => break Some(e),
                                Ok(Ctl::DefaultChanged) => {
                                    // Windows sends a few of these per switch: take them all,
                                    // then reopen on the new devices. Her queued audio carries on.
                                    while rx.recv_timeout(Duration::from_millis(250)).is_ok() {}
                                    break None;
                                }
                                Ok(Ctl::Wake) => {
                                    last_sound = Instant::now();
                                    if !on && speakers.play().is_ok() {
                                        on = true;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    if shared.playing() {
                                        last_sound = Instant::now();
                                    } else if on && last_sound.elapsed() > SPEAKER_IDLE {
                                        let _ = speakers.pause();
                                        on = false;
                                        shared.out_level.store(0f32.to_bits(), Ordering::Relaxed);
                                        shared.out_rms.store(0f32.to_bits(), Ordering::Relaxed);
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => {
                                    break Some("audio stopped".into());
                                }
                            }
                        }
                    }
                    Err(e) => Some(e),
                };
                // A device switch reopens right away; a failure (unplugged, busy)
                // retries soon, then less often while nothing works.
                if let Some(e) = failure {
                    *shared.mic_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                    std::thread::sleep(retry);
                    retry = (retry * 2).min(Duration::from_secs(5));
                }
            }
        })
        .expect("audio thread");
}

fn open_streams(
    shared: &Arc<Shared>,
    mic: mpsc::Sender<(u32, Vec<f32>)>,
    err: mpsc::Sender<Ctl>,
) -> Result<(cpal::Stream, cpal::Stream), String> {
    let host = cpal::default_host();
    let input = host.default_input_device().ok_or("No microphone found.")?;
    let output = host.default_output_device().ok_or("No speakers found.")?;
    *shared.devices.lock().unwrap_or_else(|e| e.into_inner()) = (
        input.name().unwrap_or_default(),
        output.name().unwrap_or_default(),
    );
    let in_cfg = input.default_input_config().map_err(|e| e.to_string())?;
    let out_cfg = output.default_output_config().map_err(|e| e.to_string())?;

    let in_rate = in_cfg.sample_rate().0;
    let in_ch = in_cfg.channels() as usize;
    let e1 = err.clone();
    let on_err = move |e: cpal::StreamError| {
        let _ = e1.send(Ctl::Failed(e.to_string()));
    };
    let in_stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            build_input::<f32>(&input, &in_cfg.into(), in_ch, in_rate, mic, on_err)
        }
        cpal::SampleFormat::I16 => {
            build_input::<i16>(&input, &in_cfg.into(), in_ch, in_rate, mic, on_err)
        }
        cpal::SampleFormat::U16 => {
            build_input::<u16>(&input, &in_cfg.into(), in_ch, in_rate, mic, on_err)
        }
        f => return Err(format!("Unsupported microphone format {f}.")),
    }?;

    let out_rate = out_cfg.sample_rate().0;
    shared.out_rate.store(out_rate, Ordering::Relaxed);
    *shared.down.lock().unwrap_or_else(|e| e.into_inner()) = Resampler::new(RATE, out_rate);
    shared.stop_reply();
    let out_ch = out_cfg.channels() as usize;
    let e2 = err;
    let on_err2 = move |e: cpal::StreamError| {
        let _ = e2.send(Ctl::Failed(e.to_string()));
    };
    let out_stream = match out_cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            build_output::<f32>(&output, &out_cfg.into(), out_ch, shared.clone(), on_err2)
        }
        cpal::SampleFormat::I16 => {
            build_output::<i16>(&output, &out_cfg.into(), out_ch, shared.clone(), on_err2)
        }
        cpal::SampleFormat::U16 => {
            build_output::<u16>(&output, &out_cfg.into(), out_ch, shared.clone(), on_err2)
        }
        f => return Err(format!("Unsupported speaker format {f}.")),
    }?;
    in_stream.play().map_err(|e| e.to_string())?;
    // The speakers start paused; the device thread plays them when there's audio.
    let _ = out_stream.pause();
    Ok((in_stream, out_stream))
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    tx: mpsc::Sender<(u32, Vec<f32>)>,
    on_err: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            let mono: Vec<f32> = data
                .chunks(channels.max(1))
                .map(|c| c.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / c.len() as f32)
                .collect();
            let _ = tx.send((rate, mono));
        },
        on_err,
        None,
    )
    .map_err(|e| format!("Microphone: {e}"))
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: usize,
    shared: Arc<Shared>,
    on_err: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let frames = data.len() / channels.max(1);
            let mut played = Vec::with_capacity(frames);
            if let Ok(mut q) = shared.out.try_lock() {
                for _ in 0..frames {
                    played.push(q.pop_front().unwrap_or(0.0));
                }
            } else {
                played.resize(frames, 0.0);
            }
            let gain = if shared.duck.load(Ordering::Relaxed) {
                DUCK
            } else {
                1.0
            };
            for (i, frame) in data.chunks_mut(channels.max(1)).enumerate() {
                let v = T::from_sample(played.get(i).copied().unwrap_or(0.0) * gain);
                for s in frame {
                    *s = v;
                }
            }
            let r = rms(&played);
            shared.out_rms.store(r.to_bits(), Ordering::Relaxed);
            level(&shared.out_level, r);
            if r > 0.002 {
                shared
                    .last_out_ms
                    .store(shared.now_ms().max(1), Ordering::Relaxed);
            }
        },
        on_err,
        None,
    )
    .map_err(|e| format!("Speakers: {e}"))
}

/// Windows tells us when the default speakers or mic change; the device
/// thread then reopens its streams there. A notification, not polling.
#[cfg(windows)]
mod watch {
    use super::{Ctl, Shared};
    use std::sync::Arc;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
        IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
    use windows::core::{PCWSTR, implement};

    #[implement(IMMNotificationClient)]
    struct Client(Arc<Shared>);

    impl IMMNotificationClient_Impl for Client_Impl {
        fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
            Ok(())
        }
        fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn OnDefaultDeviceChanged(
            &self,
            _: EDataFlow,
            role: ERole,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            // The default-device role is what cpal opens (communications is separate).
            let tx = (role == eConsole)
                .then(|| self.0.wake.lock().ok().and_then(|w| w.clone()))
                .flatten();
            if let Some(tx) = tx {
                let _ = tx.send(Ctl::DefaultChanged);
            }
            Ok(())
        }
        fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Registers on a thread of its own (COM's multithreaded apartment, apart
    /// from cpal's) and keeps the registration for the life of the app.
    pub fn start(shared: Arc<Shared>) {
        let _ = std::thread::Builder::new()
            .name("adrian-devices".into())
            .spawn(move || {
                // SAFETY: plain COM setup on this thread; the enumerator and the
                // client stay alive below because this thread never ends.
                let kept = unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(
                        &MMDeviceEnumerator,
                        None,
                        CLSCTX_ALL,
                    ) else {
                        return;
                    };
                    let client: IMMNotificationClient = Client(shared).into();
                    if en.RegisterEndpointNotificationCallback(&client).is_err() {
                        return;
                    }
                    (en, client)
                };
                loop {
                    std::thread::park();
                    let _ = &kept;
                }
            });
    }
}

#[cfg(not(windows))]
mod watch {
    pub fn start(_: std::sync::Arc<super::Shared>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_to_the_right_length() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.push(&vec![0.5; 4410], &mut out);
        }
        // one second in, about one second out
        assert!((out.len() as i64 - 48_000).abs() < 10, "{}", out.len());
        assert!(out.iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn gate_opens_on_speech_with_preroll_and_closes_after_quiet() {
        let mut g = Gate::new();
        let quiet = || vec![0.001f32; FRAME];
        let loud = || vec![0.2f32; FRAME];
        for _ in 0..50 {
            assert!(matches!(
                g.frame(quiet(), 0.0, None, 0.0),
                GateEvent::Nothing
            ));
        }
        assert!(matches!(
            g.frame(loud(), 0.0, None, 0.0),
            GateEvent::Nothing
        ));
        assert!(matches!(
            g.frame(loud(), 0.0, None, 0.0),
            GateEvent::Nothing
        ));
        match g.frame(loud(), 0.0, None, 0.0) {
            GateEvent::Open(pre) => assert_eq!(pre.len(), PREROLL_FRAMES),
            _ => panic!("should open"),
        }
        let mut closed_after = 0;
        for i in 0..100 {
            if matches!(g.frame(quiet(), 0.0, None, 0.0), GateEvent::Close) {
                closed_after = i + 1;
                break;
            }
        }
        assert_eq!(closed_after, CLOSE_FRAMES as usize);
    }

    #[test]
    fn her_voice_and_loud_moments_dont_deafen_it() {
        let mut g = Gate::new();
        // She talks loudly for 5 s; the mic hears her echo, which never counts.
        for _ in 0..250 {
            let e = g.frame(vec![0.15; FRAME], 0.0, None, 0.15);
            assert!(matches!(e, GateEvent::Nothing));
        }
        // A show plays for 5 s (it may open the gate now and then)...
        for _ in 0..250 {
            if let GateEvent::Open(_) = g.frame(vec![0.06; FRAME], 0.0, None, 0.0) {
                for _ in 0..CLOSE_FRAMES {
                    g.frame(vec![0.001; FRAME], 0.0, None, 0.0);
                }
            }
        }
        for _ in 0..25 {
            g.frame(vec![0.002; FRAME], 0.0, None, 0.0);
        }
        // ...and afterwards a normal speaking voice still opens it within 60 ms.
        let mut opened = false;
        for _ in 0..3 {
            opened |= matches!(
                g.frame(vec![0.04; FRAME], 0.0, None, 0.0),
                GateEvent::Open(_)
            );
        }
        assert!(opened);
    }

    #[test]
    fn talking_over_her_cuts_in_but_her_echo_does_not() {
        let mut g = Gate::new();
        for _ in 0..20 {
            g.frame(vec![0.002; FRAME], 0.0, None, 0.0);
        }
        // Her echo (0.04 expected, 0.05 heard) for a second: nothing.
        for _ in 0..50 {
            assert!(matches!(
                g.frame(vec![0.05; FRAME], 0.0, None, 0.04),
                GateEvent::Nothing
            ));
        }
        // You talking over her, clearly louder: open on the 4th frame (80 ms).
        let mut opened_at = None;
        for i in 0..6 {
            if matches!(
                g.frame(vec![0.3; FRAME], 0.0, None, 0.04),
                GateEvent::Open(_)
            ) {
                opened_at = Some(i + 1);
                break;
            }
        }
        assert_eq!(opened_at, Some(BARGE_FRAMES as usize));
        // While open, her echo alone doesn't keep it open forever.
        let mut closed = false;
        for _ in 0..(CLOSE_FRAMES + 2) {
            closed |= matches!(
                g.frame(vec![0.05; FRAME], 0.0, None, 0.04),
                GateEvent::Close
            );
        }
        assert!(closed);
    }

    #[test]
    fn push_to_talk_and_her_voice_override_loudness() {
        let mut g = Gate::new();
        assert!(matches!(
            g.frame(vec![0.0; FRAME], 0.0, Some(true), 0.0),
            GateEvent::Open(_)
        ));
        assert!(matches!(
            g.frame(vec![0.0; FRAME], 0.0, Some(true), 0.0),
            GateEvent::Frame(_)
        ));
        assert!(matches!(
            g.frame(vec![0.5; FRAME], 0.0, Some(false), 0.0),
            GateEvent::Close
        ));
        for _ in 0..10 {
            assert!(matches!(
                g.frame(vec![0.5; FRAME], 0.0, Some(false), 0.0),
                GateEvent::Nothing
            ));
        }
    }
}
