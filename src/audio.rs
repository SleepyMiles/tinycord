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
use std::time::{Duration, Instant};

pub const RATE: usize = 48_000;
const MS: usize = RATE / 1000;

/// Max audio waiting to be sent (anything older is dropped).
const MIC_MAX: usize = 60 * MS;
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
        })
    }

    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Drop everything queued for sending except a short pre-roll, so the
    /// first syllable isn't cut when voice activation / push-to-talk opens.
    pub fn trim_mic_preroll(&self) {
        let mut m = self.mic.lock();
        let keep = 20 * MS;
        while m.len() > keep {
            m.pop_front();
        }
    }

    /// Queue decoded voice (48 kHz mono i16) for playback.
    pub fn push_out(&self, pcm: &[i16]) {
        let mut o = self.out.lock();
        o.extend(pcm.iter().map(|s| *s as f32 / 32768.0));
        if o.len() > OUT_MAX {
            // Skip ahead, blending 2 ms of old into new so there's no click.
            const XF: usize = 2 * MS;
            let old: Vec<f32> = o.iter().take(XF).copied().collect();
            let drop = o.len() - OUT_TARGET;
            o.drain(..drop);
            for (i, (n, a)) in o.iter_mut().zip(old.iter()).enumerate() {
                let t = (i + 1) as f32 / (XF + 1) as f32;
                *n = *a * (1.0 - t) + *n * t;
            }
        }
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
        // Speaking level auto gain aims for (-15 dBFS RMS, a loud, clear
        // voice), and its gain limits (up to +24 dB).
        const TARGET_RMS: f32 = 0.178;
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
            // Peak limiter: if a sample would go over 0.9, turn down at once
            // and recover over ~50 ms. Avoids the distortion of clipping.
            let mut sent = 0f32;
            for s in self.frame_out.iter() {
                let x = s * self.gain;
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

/// Leaves normal levels alone and bends peaks above 0.8 smoothly toward 1.0
/// instead of hard clipping.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        let over = a - 0.8;
        x.signum() * (0.8 + 0.2 * over / (over + 0.2))
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
    let mut mono: Vec<f32> = Vec::new();
    let mut at48: Vec<f32> = Vec::new();
    let mut proc = MicProcessor::new();
    let mut handle = move |data: &[f32]| {
        mono.clear();
        for frame in data.chunks(ch) {
            mono.push(frame.iter().sum::<f32>() / ch as f32);
        }
        at48.clear();
        rs.run(&mono, &mut at48);
        let mut m = shared.mic.lock();
        proc.run(&at48, &mut m, &shared);
        if m.len() > MIC_MAX {
            let drop = m.len() - MIC_MAX;
            m.drain(..drop);
        }
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
}

impl MicSource {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, pending: Vec::new() }
    }
}

impl std::io::Read for MicSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            let want = (buf.len() / 4).clamp(1, 20 * MS);
            // Give the mic up to ~15 ms to catch up before padding.
            let deadline = Instant::now() + Duration::from_millis(15);
            loop {
                if self.shared.mic.lock().len() >= want || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let mut m = self.shared.mic.lock();
            let n = want.min(m.len());
            for s in m.drain(..n) {
                self.pending.extend_from_slice(&s.to_le_bytes());
            }
            drop(m);
            for _ in n..want {
                self.pending.extend_from_slice(&0f32.to_le_bytes());
            }
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
                next_push += 20 + (rng % 17) as i64 - 8;
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
