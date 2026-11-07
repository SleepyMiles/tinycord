//! Microphone capture and speaker playback.
//!
//! Everything inside runs at 48 kHz mono. The sound card's own rate and channel
//! count are converted at the edges. Both buffers are kept short on purpose:
//! when they grow (clock drift, a hiccup), old audio is dropped instead of
//! letting delay pile up. That is the main fix for the "voice is seconds late"
//! problem.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use nnnoiseless::DenoiseState;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

pub const RATE: usize = 48_000;
const MS: usize = RATE / 1000;

/// Max audio waiting to be sent (anything older is dropped, with a crossfade).
const MIC_MAX: usize = 80 * MS;
/// Above this much mic backlog, the capture side runs 0.5% "faster".
const MIC_CATCHUP: usize = 45 * MS;
/// Cushion kept when sending (re)starts, so reads never come up short.
const MIC_PREROLL: usize = 30 * MS;
/// Max audio waiting to be played before we skip ahead (with a crossfade).
const OUT_MAX: usize = 150 * MS;
/// What we skip ahead to when OUT_MAX is exceeded.
const OUT_TARGET: usize = 40 * MS;
/// Audio collected before playback (re)starts, so it never runs dry mid-word.
const OUT_PREBUF: usize = 30 * MS;
/// Above this much backlog, play 0.5% faster (inaudible) to catch up.
const OUT_CATCHUP: usize = 50 * MS;

pub struct Shared {
    pub mic: Mutex<VecDeque<f32>>,
    pub out: Mutex<VecDeque<f32>>,
    /// Mic level of the last chunk, in dBFS * -1 (so 40 = -40 dBFS), as bits of f32.
    pub mic_level: AtomicU32,
    /// Milliseconds since `epoch` when voice was last heard on the mic.
    pub last_voice_ms: AtomicU64,
    pub epoch: Instant,
    /// Voice activity threshold in dBFS (stored as f32 bits).
    pub vad_db: AtomicU32,
    /// RNNoise noise suppression on/off.
    pub noise_suppression: AtomicBool,
    /// Automatic gain control on/off.
    pub auto_gain: AtomicBool,
    /// Current auto-gain multiplier (f32 bits), shown in the title bar.
    pub agc_gain: AtomicU32,
    /// Input volume multiplier 0.0..=2.0 (f32 bits), like Discord's slider.
    pub input_volume: AtomicU32,
    /// Mic level the sensitivity slider compares against (dBFS, f32 bits):
    /// after noise suppression, before gain.
    pub vad_level: AtomicU32,
    /// true = let mic audio through; false = fade it out (end of talking).
    pub gate_open: AtomicBool,
    /// Set once the fade-out has reached silence.
    pub faded_out: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mic: Mutex::new(VecDeque::with_capacity(MIC_MAX * 2)),
            out: Mutex::new(VecDeque::with_capacity(OUT_MAX * 2)),
            mic_level: AtomicU32::new(0f32.to_bits()),
            last_voice_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            vad_db: AtomicU32::new((-45.0f32).to_bits()),
            noise_suppression: AtomicBool::new(true),
            auto_gain: AtomicBool::new(true),
            agc_gain: AtomicU32::new(1f32.to_bits()),
            input_volume: AtomicU32::new(1f32.to_bits()),
            vad_level: AtomicU32::new((-100f32).to_bits()),
            gate_open: AtomicBool::new(false),
            faded_out: AtomicBool::new(true),
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Drop everything queued for sending except a short pre-roll, so the
    /// first syllable isn't cut when voice activation / push-to-talk opens.
    pub fn trim_mic_preroll(&self) {
        let mut m = self.mic.lock();
        if m.len() > MIC_PREROLL {
            let drop = m.len() - MIC_PREROLL;
            m.drain(..drop);
        }
    }

    /// Queue decoded voice (48 kHz mono i16) for playback.
    pub fn push_out(&self, pcm: &[i16]) {
        let mut o = self.out.lock();
        o.extend(pcm.iter().map(|s| *s as f32 / 32768.0));
        trim_with_crossfade(&mut o, OUT_MAX, OUT_TARGET);
    }
}

/// If `q` holds more than `max`, cut it down to `target`, blending 2 ms of the
/// old start into the new start so the cut doesn't click.
fn trim_with_crossfade(q: &mut VecDeque<f32>, max: usize, target: usize) {
    if q.len() <= max {
        return;
    }
    const XF: usize = 2 * MS;
    let old: Vec<f32> = q.iter().take(XF).copied().collect();
    let drop = q.len() - target;
    q.drain(..drop);
    for (i, (n, a)) in q.iter_mut().zip(old.iter()).enumerate() {
        let t = (i + 1) as f32 / (XF + 1) as f32;
        *n = *a * (1.0 - t) + *n * t;
    }
}

/// Simple linear resampler that keeps its position between calls.
struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, last: 0.0 }
    }

    /// Feed `input`, append converted samples to `out`.
    fn run(&mut self, input: &[f32], out: &mut impl Extend<f32>) {
        if (self.step - 1.0).abs() < 1e-9 && self.pos.fract() == 0.0 {
            // Same rate and lined up on a sample: copy straight through.
            // Keep `last`/`pos` correct so switching modes stays seamless.
            let start = self.pos as usize; // 0 or 1 (1 = `last` is next)
            if start == 0 {
                out.extend(std::iter::once(self.last).chain(input[..input.len().saturating_sub(1)].iter().copied()));
            } else {
                out.extend(input.iter().copied());
                self.pos = 1.0;
            }
            if let Some(l) = input.last() {
                self.last = *l;
            }
            if start == 0 {
                self.pos = 0.0;
            }
            return;
        }
        let mut res = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        // Position is measured with index -1 = self.last.
        while self.pos < input.len() as f64 {
            let i = self.pos.floor() as isize - 1;
            let frac = (self.pos - self.pos.floor()) as f32;
            let a = if i < 0 { self.last } else { input[i as usize] };
            let b = input[(i + 1) as usize];
            res.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        if let Some(l) = input.last() {
            self.last = *l;
        }
        out.extend(res);
    }
}

/// Mic clean-up, run in 10 ms frames at 48 kHz:
/// 1. RNNoise noise suppression (also gives a "is this a voice?" score),
/// 2. voice activity detection,
/// 3. automatic gain toward a steady speaking level, with a soft limiter.
struct MicProcessor {
    ns: Box<DenoiseState<'static>>,
    pending: Vec<f32>,
    frame_in: [f32; DenoiseState::FRAME_SIZE],
    frame_out: [f32; DenoiseState::FRAME_SIZE],
    gain: f32,
    limit: f32,
}

impl MicProcessor {
    fn new() -> Self {
        Self {
            ns: DenoiseState::new(),
            pending: Vec::with_capacity(4 * DenoiseState::FRAME_SIZE),
            frame_in: [0.0; DenoiseState::FRAME_SIZE],
            frame_out: [0.0; DenoiseState::FRAME_SIZE],
            gain: 1.0,
            limit: 1.0,
        }
    }

    fn run(&mut self, input: &[f32], out: &mut VecDeque<f32>, shared: &Shared) {
        const N: usize = DenoiseState::FRAME_SIZE;
        // Speaking level auto gain aims for (-16.5 dBFS RMS, loud but with
        // room for peaks), and its gain limits (up to +24 dB).
        const TARGET_RMS: f32 = 0.15;
        const MIN_GAIN: f32 = 0.5;
        const MAX_GAIN: f32 = 16.0;

        self.pending.extend_from_slice(input);
        let ns_on = shared.noise_suppression.load(Ordering::Relaxed);
        let agc_on = shared.auto_gain.load(Ordering::Relaxed);
        let threshold = f32::from_bits(shared.vad_db.load(Ordering::Relaxed));
        let mut start = 0;
        while self.pending.len() - start >= N {
            let chunk = &self.pending[start..start + N];
            start += N;
            // RNNoise works on 16-bit-scale floats.
            for (d, s) in self.frame_in.iter_mut().zip(chunk) {
                *d = s * 32768.0;
            }
            let vad_prob = if ns_on {
                self.ns.process_frame(&mut self.frame_out, &self.frame_in)
            } else {
                self.frame_out.copy_from_slice(&self.frame_in);
                1.0
            };

            let mut sum = 0f32;
            for s in self.frame_out.iter_mut() {
                *s /= 32768.0;
                sum += *s * *s;
            }
            let rms = (sum / N as f32).sqrt();
            let db = 20.0 * rms.max(1e-9).log10();
            shared.vad_level.store(db.to_bits(), Ordering::Relaxed);
            let voice = db > threshold && (!ns_on || vad_prob > 0.5);
            if voice {
                shared.last_voice_ms.store(shared.now_ms(), Ordering::Relaxed);
            }

            if agc_on {
                if voice && rms > 1e-4 {
                    let want = (TARGET_RMS / rms).clamp(MIN_GAIN, MAX_GAIN);
                    // Turn down quickly, turn up slowly (avoids pumping).
                    let rate = if want < self.gain { 0.25 } else { 0.05 };
                    self.gain += (want - self.gain) * rate;
                }
            } else {
                self.gain = 1.0;
            }
            shared.agc_gain.store(self.gain.to_bits(), Ordering::Relaxed);
            let volume = f32::from_bits(shared.input_volume.load(Ordering::Relaxed)).clamp(0.0, 2.0);
            // Peak limiter: if a sample would go over 0.9, turn down at once
            // and recover over ~50 ms. Avoids the distortion of clipping.
            let mut sent = 0f32;
            for s in self.frame_out.iter() {
                let x = s * self.gain * volume;
                let need = if x.abs() * self.limit > 0.9 { 0.9 / x.abs() } else { 1.0 };
                if need < self.limit {
                    self.limit = need;
                } else {
                    self.limit += (1.0 - self.limit) * 0.0005;
                }
                let y = soft_limit(x * self.limit);
                sent += y * y;
                out.push_back(y);
            }
            let sent_db = 10.0 * (sent / N as f32).max(1e-18).log10();
            shared.mic_level.store(sent_db.to_bits(), Ordering::Relaxed);
        }
        self.pending.drain(..start);
    }
}

/// Last-resort safety: leaves everything up to 0.92 untouched (the limiter
/// keeps peaks at 0.9) and bends anything above smoothly toward 1.0.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.92 {
        x
    } else {
        let over = a - 0.92;
        x.signum() * (0.92 + 0.08 * over / (over + 0.08))
    }
}

/// Turns the 48 kHz voice queue into device audio without clicks: waits for
/// a small cushion before starting, fades in, lets the sound die away if the
/// queue runs dry, and plays 0.5% faster when audio piles up.
struct Playout {
    rs: Resampler,
    base_step: f64,
    conv: VecDeque<f32>,
    tmp: Vec<f32>,
    playing: bool,
    fade: f32,
    last: f32,
    fade_step: f32,
}

impl Playout {
    fn new(device_rate: u32) -> Self {
        let rs = Resampler::new(RATE as u32, device_rate);
        Self {
            base_step: rs.step,
            rs,
            conv: VecDeque::new(),
            tmp: Vec::new(),
            playing: false,
            fade: 0.0,
            last: 0.0,
            fade_step: 1.0 / (device_rate as f32 * 0.003),
        }
    }

    fn fill(&mut self, out: &mut [f32], ch: usize, shared: &Shared, deaf: bool) {
        let frames = out.len() / ch;
        let backlog = shared.out.lock().len();
        if !self.playing && backlog >= OUT_PREBUF {
            self.playing = true;
            self.fade = 0.0;
        }
        self.rs.step = if backlog > OUT_CATCHUP { self.base_step * 1.005 } else { self.base_step };
        while self.playing && self.conv.len() < frames {
            let need = ((frames - self.conv.len()) as f64 * self.rs.step).ceil() as usize + 1;
            self.tmp.clear();
            {
                let mut o = shared.out.lock();
                let n = need.min(o.len());
                self.tmp.extend(o.drain(..n));
            }
            if self.tmp.is_empty() {
                break;
            }
            self.rs.run(&self.tmp, &mut self.conv);
        }
        for frame in out.chunks_mut(ch) {
            let next = if self.playing { self.conv.pop_front() } else { None };
            let s = match next {
                Some(s) => {
                    self.fade = (self.fade + self.fade_step).min(1.0);
                    s * self.fade
                }
                None => {
                    // Ran dry: let the last value die away instead of
                    // jumping to zero (that jump is the "pop").
                    self.playing = false;
                    self.last * 0.97
                }
            };
            self.last = s;
            let s = if deaf { 0.0 } else { s.clamp(-1.0, 1.0) };
            for x in frame.iter_mut() {
                *x = s;
            }
        }
    }
}

pub struct AudioHandle {
    stop: mpsc::Sender<()>,
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

/// Opens the default mic and speakers on a thread of their own (cpal streams
/// must stay on the thread that made them). Dropping the handle closes both.
/// Names of the input and output devices Windows offers.
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |it: Option<Vec<cpal::Device>>| -> Vec<String> {
        let mut v: Vec<String> = it
            .unwrap_or_default()
            .iter()
            .filter_map(|d| d.description().ok().map(|x| x.name().to_string()))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let ins = names(host.input_devices().ok().map(|i| i.collect()));
    let outs = names(host.output_devices().ok().map(|i| i.collect()));
    (ins, outs)
}

fn find_device(want: &Option<String>, input: bool) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = want {
        let found = if input {
            host.input_devices().ok().and_then(|mut it| {
                it.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
            })
        } else {
            host.output_devices().ok().and_then(|mut it| {
                it.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
            })
        };
        if found.is_some() {
            return found;
        }
        tracing::warn!("device {name:?} not found, using the default");
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

pub fn start(
    shared: Arc<Shared>,
    deafened: Arc<AtomicBool>,
    mic: Option<String>,
    spk: Option<String>,
) -> AudioHandle {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("audio".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let host = cpal::default_host();
            let _ = &host;
            let input = find_device(&mic, true).and_then(|d| build_input(&d, shared.clone()));
            let output =
                find_device(&spk, false).and_then(|d| build_output(&d, shared.clone(), deafened.clone()));
            match (&input, &output) {
                (None, _) => crate::state::set_status("No microphone found"),
                (_, None) => crate::state::set_status("No speakers found"),
                _ => {}
            }
            // Park until told to stop; streams are dropped on return.
            let _ = rx.recv();
            drop(input);
            drop(output);
        })
        .expect("audio thread");
    AudioHandle { stop: tx }
}

fn build_input(dev: &cpal::Device, shared: Arc<Shared>) -> Option<cpal::Stream> {
    let sup = dev.default_input_config().ok()?;
    tracing::info!("mic: {:?} {:?}", dev.description().ok(), sup);
    let fmt = sup.sample_format();
    let cfg = sup.config();
    let ch = cfg.channels.max(1) as usize;
    let mut rs = Resampler::new(cfg.sample_rate, RATE as u32);
    let base_step = rs.step;
    let mut mono: Vec<f32> = Vec::new();
    let mut at48: Vec<f32> = Vec::new();
    let mut proc = MicProcessor::new();
    let mut handle = move |data: &[f32]| {
        mono.clear();
        for frame in data.chunks(ch) {
            mono.push(frame.iter().sum::<f32>() / ch as f32);
        }
        let backlog = shared.mic.lock().len();
        rs.step = if backlog > MIC_CATCHUP { base_step * 1.005 } else { base_step };
        at48.clear();
        rs.run(&mono, &mut at48);
        let mut m = shared.mic.lock();
        proc.run(&at48, &mut m, &shared);
        trim_with_crossfade(&mut m, MIC_MAX, MIC_PREROLL);
    };
    let err = |e| eprintln!("mic error: {e}");
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            dev.build_input_stream::<f32, _, _>(cfg, move |d, _| handle(d), err, None).ok()?
        }
        cpal::SampleFormat::I16 => {
            let mut buf = Vec::new();
            dev.build_input_stream::<i16, _, _>(
                cfg,
                move |d, _| {
                    buf.clear();
                    buf.extend(d.iter().map(|s| *s as f32 / 32768.0));
                    handle(&buf)
                },
                err,
                None,
            )
            .ok()?
        }
        _ => return None,
    };
    stream.play().ok()?;
    Some(stream)
}

fn build_output(
    dev: &cpal::Device,
    shared: Arc<Shared>,
    deafened: Arc<AtomicBool>,
) -> Option<cpal::Stream> {
    let sup = dev.default_output_config().ok()?;
    tracing::info!("speakers: {:?} {:?}", dev.description().ok(), sup);
    let fmt = sup.sample_format();
    let cfg = sup.config();
    let ch = cfg.channels.max(1) as usize;
    let mut playout = Playout::new(cfg.sample_rate);
    let mut fill = move |out: &mut [f32]| {
        let deaf = deafened.load(Ordering::Relaxed);
        playout.fill(out, ch, &shared, deaf);
    };
    let err = |e| eprintln!("speaker error: {e}");
    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            dev.build_output_stream::<f32, _, _>(cfg, move |d, _| fill(d), err, None).ok()?
        }
        cpal::SampleFormat::I16 => {
            let mut buf = Vec::new();
            dev.build_output_stream::<i16, _, _>(
                cfg,
                move |d: &mut [i16], _| {
                    buf.resize(d.len(), 0.0);
                    fill(&mut buf);
                    for (o, s) in d.iter_mut().zip(buf.iter()) {
                        *o = (s * 32767.0) as i16;
                    }
                },
                err,
                None,
            )
            .ok()?
        }
        _ => return None,
    };
    stream.play().ok()?;
    Some(stream)
}

/// The mic as a byte stream songbird can read (raw f32 LE, 48 kHz mono).
/// Never blocks for long and never ends: if the mic is behind, it pads with
/// silence instead of letting songbird wait.
pub struct MicSource {
    pub shared: Arc<Shared>,
    pending: Vec<u8>,
    last: f32,
    short_reads: u64,
    /// 0..1 fade applied to what we send (see `Shared::gate_open`).
    env: f32,
}

impl MicSource {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, pending: Vec::new(), last: 0.0, short_reads: 0, env: 0.0 }
    }
}

impl std::io::Read for MicSource {
    /// Called by the voice engine once per 20 ms packet. Never waits: if the
    /// mic is a little behind, the gap is filled by letting the last sample
    /// fade out (no hard silence, which is what made a "pop").
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            let want = (buf.len() / 4).clamp(1, 20 * MS);
            let mut samples: Vec<f32> = Vec::with_capacity(want);
            {
                let mut m = self.shared.mic.lock();
                let n = want.min(m.len());
                samples.extend(m.drain(..n));
            }
            if let Some(l) = samples.last() {
                self.last = *l;
            }
            if samples.len() < want {
                self.short_reads += 1;
                if self.short_reads % 50 == 1 {
                    tracing::info!("mic short read #{} ({} of {} samples)", self.short_reads, samples.len(), want);
                }
                while samples.len() < want {
                    self.last *= 0.99;
                    samples.push(self.last);
                }
            }
            // Fade in over 5 ms when sending starts, out over 20 ms when it
            // stops, so a sentence never ends mid-waveform (the end "pop").
            let open = self.shared.gate_open.load(Ordering::Relaxed);
            let (target, step) = if open { (1.0, 1.0 / (5 * MS) as f32) } else { (0.0, 1.0 / (20 * MS) as f32) };
            for x in samples.iter_mut() {
                if self.env < target {
                    self.env = (self.env + step).min(target);
                } else if self.env > target {
                    self.env = (self.env - step).max(target);
                }
                *x *= self.env;
                self.pending.extend_from_slice(&x.to_le_bytes());
            }
            self.shared.faded_out.store(!open && self.env == 0.0, Ordering::Relaxed);
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

impl std::io::Seek for MicSource {
    fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "live mic"))
    }
}

impl songbird::input::core::io::MediaSource for MicSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn noise(n: usize, amp: f32) -> Vec<f32> {
        let mut x: u32 = 12345;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Voice arrives in 20 ms chunks with up to 8 ms of timing jitter; the
    /// sound card pulls 10 ms at a time. Output must have no clicks.
    #[test]
    fn playback_has_no_clicks() {
        let sh = Shared::new();
        let mut p = Playout::new(48_000);
        let tone = |n: usize| ((n as f32) * 200.0 * 6.283 / 48_000.0).sin() * 0.5;
        let mut sent = 0usize;
        let mut out_all = Vec::new();
        let mut next_push = 0i64;
        let mut rng: u32 = 7;
        for ms in 0..5000i64 {
            if ms >= next_push {
                let pcm: Vec<i16> = (sent..sent + 960).map(|n| (tone(n) * 32767.0) as i16).collect();
                sent += 960;
                sh.push_out(&pcm);
                rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
                // Ticks are on a steady 20 ms clock; jitter doesn't add up.
                next_push = (sent / 960) as i64 * 20 + (rng % 17) as i64 - 8;
            }
            if ms % 10 == 0 {
                let mut buf = vec![0f32; 480];
                p.fill(&mut buf, 1, &sh, false);
                out_all.extend(buf);
            }
        }
        let start = out_all.iter().position(|x| x.abs() > 0.01).unwrap();
        let body = &out_all[start + 200..];
        let max_jump = body.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        let silent_gaps = body.windows(240).filter(|w| w.iter().all(|x| x.abs() < 1e-3)).count();
        println!("max jump {max_jump:.4} (a smooth tone moves at most 0.013), silent 5ms gaps {silent_gaps}, backlog {} ms", sh.out.lock().len() / MS);
        assert!(max_jump < 0.03, "click detected");
        assert_eq!(silent_gaps, 0);
    }

    /// Mic delivers 10 ms chunks with up to 4 ms of jitter; the voice engine
    /// reads 20 ms every 20 ms. What we send must have no clicks or gaps.
    #[test]
    fn sending_has_no_clicks() {
        use std::io::Read;
        let sh = Shared::new();
        sh.noise_suppression.store(false, Ordering::Relaxed);
        sh.auto_gain.store(false, Ordering::Relaxed);
        let mut proc = MicProcessor::new();
        let mut src = MicSource::new(sh.clone());
        let tone = |n: usize| ((n as f32) * 200.0 * 6.283 / 48_000.0).sin() * 0.5;
        let mut made = 0usize;
        let mut sent: Vec<f32> = Vec::new();
        let mut next_cap = 0i64;
        let mut rng: u32 = 99;
        let mut started = false;
        for ms in 0..5000i64 {
            if ms >= next_cap {
                let chunk: Vec<f32> = (made..made + 480).map(tone).collect();
                made += 480;
                let mut m = sh.mic.lock();
                proc.run(&chunk, &mut m, &sh);
                trim_with_crossfade(&mut m, MIC_MAX, MIC_PREROLL);
                drop(m);
                rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5;
                // The sound card's clock is steady: jitter doesn't add up.
                next_cap = (made / 480) as i64 * 10 + (rng % 9) as i64 - 4;
            }
            if ms == 200 {
                sh.trim_mic_preroll(); // "start sending"
                sh.gate_open.store(true, Ordering::Relaxed);
                started = true;
            }
            if started && ms % 20 == 0 {
                let mut buf = vec![0u8; 3840];
                let mut got = 0;
                while got < buf.len() {
                    got += src.read(&mut buf[got..]).unwrap();
                }
                sent.extend(buf.chunks(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            }
        }
        let short_while_talking = src.short_reads;
        // Stop talking: the gate closes and the voice must fade, not cut.
        sh.gate_open.store(false, Ordering::Relaxed);
        for _ in 0..3 {
            let mut buf = vec![0u8; 3840];
            src.read(&mut buf).unwrap();
            sent.extend(buf.chunks(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        }
        assert!(sh.faded_out.load(Ordering::Relaxed));
        assert!(sent.last().unwrap().abs() < 1e-6);
        let body = &sent[960..];
        let max_jump = body.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        println!("send: max jump {max_jump:.4} (smooth tone: 0.013), short reads {short_while_talking}, faded to silence at the end");
        assert!(max_jump < 0.03, "click in sent audio");
        assert_eq!(short_while_talking, 0);
    }

    #[test]
    fn noise_is_suppressed() {
        let sh = Shared::new();
        sh.auto_gain.store(false, Ordering::Relaxed);
        let mut p = MicProcessor::new();
        let input = noise(RATE * 2, 0.05);
        let mut out = VecDeque::new();
        let t = Instant::now();
        p.run(&input, &mut out, &sh);
        let took = t.elapsed();
        let out: Vec<f32> = out.into_iter().collect();
        let tail = &out[RATE..];
        println!("noise rms in {:.4} out {:.4}; 2 s processed in {:?}", rms(&input[RATE..]), rms(tail), took);
        assert!(rms(tail) < rms(&input[RATE..]) * 0.75);
    }

    #[test]
    fn quiet_voice_is_boosted() {
        let sh = Shared::new();
        sh.noise_suppression.store(false, Ordering::Relaxed);
        let mut p = MicProcessor::new();
        // A quiet "voice": 150 Hz tone with harmonics around -38 dBFS.
        let input: Vec<f32> = (0..RATE * 3)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                0.008 * ((t * 150.0 * 6.283).sin() + 0.5 * (t * 300.0 * 6.283).sin())
            })
            .collect();
        let mut out = VecDeque::new();
        p.run(&input, &mut out, &sh);
        let out: Vec<f32> = out.into_iter().collect();
        println!("quiet rms in {:.4} out {:.4} gain {:.2}", rms(&input[RATE * 2..]), rms(&out[RATE * 2..]), p.gain);
        assert!(p.gain > 3.0);
    }

    #[test]
    fn loud_voice_is_not_clipped() {
        let sh = Shared::new();
        sh.noise_suppression.store(false, Ordering::Relaxed);
        let mut p = MicProcessor::new();
        let input: Vec<f32> = (0..RATE * 2).map(|i| 0.99 * (i as f32 / RATE as f32 * 200.0 * 6.283).sin()).collect();
        let mut out = VecDeque::new();
        p.run(&input, &mut out, &sh);
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        println!("loud peak out {peak:.3} gain {:.2}", p.gain);
        assert!(peak <= 1.0 && p.gain < 1.0);
    }
}
