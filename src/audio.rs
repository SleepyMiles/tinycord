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
    /// RNNoise strength 0.1..=1.0 when on (f32 bits). Lower = more of your
    /// natural voice kept, fewer "processed"/watery artifacts.
    pub ns_strength: AtomicU32,
    /// Gentle presence boost for clearer speech.
    pub clarity: AtomicBool,
    /// When the raw mic last hit full scale (ms since epoch), 0 = never.
    pub last_clip_ms: AtomicU64,
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
            ns_strength: AtomicU32::new(0.8f32.to_bits()),
            clarity: AtomicBool::new(false),
            last_clip_ms: AtomicU64::new(0),
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

/// Sample-rate converter. Same rate: the cheap path below (exact copy, or a
/// tiny 0.5% speed nudge when catching up). Different rates (e.g. a 44.1 kHz
/// mic): a windowed-sinc resampler, which keeps the crisp high end of your
/// voice that straight-line interpolation dulls.
struct Converter {
    lin: Resampler,
    base_step: f64,
    sinc: Option<rubato::SincFixedIn<f32>>,
    inbuf: Vec<f32>,
    fast: bool,
}

impl Converter {
    fn new(from: u32, to: u32) -> Self {
        let lin = Resampler::new(from, to);
        let sinc = if from == to {
            None
        } else {
            use rubato::{SincInterpolationParameters, SincInterpolationType, WindowFunction};
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            rubato::SincFixedIn::<f32>::new(to as f64 / from as f64, 1.02, params, 240, 1).ok()
        };
        Self { base_step: lin.step, lin, sinc, inbuf: Vec::new(), fast: false }
    }

    /// Input samples consumed per output sample (roughly).
    fn step(&self) -> f64 {
        self.lin.step
    }

    /// Run 0.5% fast (consume input a little quicker) to shrink a backlog.
    fn set_fast(&mut self, fast: bool) {
        if fast == self.fast {
            return;
        }
        self.fast = fast;
        let f = if fast { 1.005 } else { 1.0 };
        self.lin.step = self.base_step * f;
        if let Some(r) = self.sinc.as_mut() {
            use rubato::Resampler as _;
            let _ = r.set_resample_ratio_relative(1.0 / f, true);
        }
    }

    fn run(&mut self, input: &[f32], out: &mut impl Extend<f32>) {
        let Some(r) = self.sinc.as_mut() else {
            self.lin.run(input, out);
            return;
        };
        use rubato::Resampler as _;
        self.inbuf.extend_from_slice(input);
        loop {
            let need = r.input_frames_next();
            if self.inbuf.len() < need {
                break;
            }
            if let Ok(res) = r.process(&[&self.inbuf[..need]], None) {
                out.extend(res[0].iter().copied());
            }
            self.inbuf.drain(..need);
        }
    }
}

/// Second-order filter (RBJ cookbook). Used for the 80 Hz high-pass (removes
/// breath rumble, pops and desk thumps) and the optional clarity shelf.
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    fn from(b: [f32; 3], a0: f32, a1: f32, a2: f32) -> Self {
        Self { b: [b[0] / a0, b[1] / a0, b[2] / a0], a: [a1 / a0, a2 / a0], x: [0.0; 2], y: [0.0; 2] }
    }

    fn high_pass(cutoff: f32, rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * cutoff / rate;
        let alpha = w0.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let c = w0.cos();
        Self::from([(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0], 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// Boost (or cut) everything above `freq` by `db`.
    fn high_shelf(freq: f32, db: f32, rate: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let (sn, c) = (w0.sin(), w0.cos());
        let alpha = sn / 2.0 * 2f32.sqrt(); // shelf slope S = 1
        let sa = 2.0 * a.sqrt() * alpha;
        Self::from(
            [
                a * ((a + 1.0) + (a - 1.0) * c + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - sa),
            ],
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    fn run(&mut self, v: f32) -> f32 {
        let y = self.b[0] * v + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [v, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// Voice leveler (compressor): evens out loud and soft syllables smoothly,
/// so the voice can be louder overall without the limiter squashing peaks.
/// 3:1 above -22 dBFS with a soft knee, 5 ms attack, 150 ms release, +5 dB
/// makeup.
struct Compressor {
    env: f32,
    gr_db: f32,
    c_env: f32,
    c_att: f32,
    c_rel: f32,
}

impl Compressor {
    const THRESHOLD: f32 = -22.0;
    const RATIO: f32 = 3.0;
    const KNEE: f32 = 6.0;
    const MAKEUP: f32 = 5.0;

    fn new(rate: f32) -> Self {
        let c = |sec: f32| 1.0 - (-1.0 / (sec * rate)).exp();
        Self { env: 0.0, gr_db: 0.0, c_env: c(0.010), c_att: c(0.005), c_rel: c(0.150) }
    }

    /// Gain (linear) to apply to sample `x`.
    fn gain(&mut self, x: f32) -> f32 {
        self.env += (x * x - self.env) * self.c_env;
        let lvl = 10.0 * (self.env + 1e-12).log10();
        let over = lvl - Self::THRESHOLD;
        let slope = 1.0 / Self::RATIO - 1.0;
        let want = if over <= -Self::KNEE / 2.0 {
            0.0
        } else if over < Self::KNEE / 2.0 {
            slope * (over + Self::KNEE / 2.0).powi(2) / (2.0 * Self::KNEE)
        } else {
            slope * over
        };
        let c = if want < self.gr_db { self.c_att } else { self.c_rel };
        self.gr_db += (want - self.gr_db) * c;
        10f32.powf((self.gr_db + Self::MAKEUP) / 20.0)
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
    /// Auto-gain multiplier (changes slowly, per 10 ms frame).
    gain: f32,
    /// Gain actually applied at the end of the last frame; the next frame
    /// glides from here, so gain never jumps between samples.
    applied: f32,
    /// Smoothed speech power that auto gain reacts to (~70 ms memory).
    speech_pow: f32,
    /// Look-ahead limiter: samples waiting 2 ms, and how far each one would
    /// need turning down to stay under the ceiling.
    delay: VecDeque<(f32, f32)>,
    limit: f32,
    hpf: Biquad,
    shelf: Biquad,
    comp: Compressor,
    /// Last input frame, to blend with RNNoise's output (which is one
    /// frame behind) when strength < 100%.
    prev_in: [f32; DenoiseState::FRAME_SIZE],
}

/// Limiter look-ahead (2 ms) and ceiling.
const LOOKAHEAD: usize = 2 * MS;
const CEILING: f32 = 0.9;

impl MicProcessor {
    fn new() -> Self {
        Self {
            ns: DenoiseState::new(),
            pending: Vec::with_capacity(4 * DenoiseState::FRAME_SIZE),
            frame_in: [0.0; DenoiseState::FRAME_SIZE],
            frame_out: [0.0; DenoiseState::FRAME_SIZE],
            gain: 1.0,
            applied: 1.0,
            speech_pow: 0.0,
            delay: VecDeque::with_capacity(LOOKAHEAD + 1),
            limit: 1.0,
            hpf: Biquad::high_pass(80.0, RATE as f32),
            shelf: Biquad::high_shelf(3000.0, 4.0, RATE as f32),
            comp: Compressor::new(RATE as f32),
            prev_in: [0.0; DenoiseState::FRAME_SIZE],
        }
    }

    fn run(&mut self, input: &[f32], out: &mut VecDeque<f32>, shared: &Shared) {
        const N: usize = DenoiseState::FRAME_SIZE;
        // Auto gain aims for a speaking level of -16.5 dBFS RMS (loud, with
        // room for peaks), boosting up to +30 dB for soft, breathy speech.
        const TARGET_RMS: f32 = 0.15;
        const MIN_GAIN: f32 = 0.5;
        const MAX_GAIN: f32 = 31.6;

        self.pending.extend_from_slice(input);
        let ns_on = shared.noise_suppression.load(Ordering::Relaxed);
        let agc_on = shared.auto_gain.load(Ordering::Relaxed);
        let threshold = f32::from_bits(shared.vad_db.load(Ordering::Relaxed));
        let volume = f32::from_bits(shared.input_volume.load(Ordering::Relaxed)).clamp(0.0, 2.0);
        let mut start = 0;
        while self.pending.len() - start >= N {
            let chunk = &self.pending[start..start + N];
            start += N;
            // RNNoise works on 16-bit-scale floats.
            for (d, s) in self.frame_in.iter_mut().zip(chunk) {
                *d = s * 32768.0;
            }
            let vad_prob = if ns_on {
                let p = self.ns.process_frame(&mut self.frame_out, &self.frame_in);
                // Keep some of the natural voice: full-strength suppression
                // is what sounds watery/"processed".
                let k = f32::from_bits(shared.ns_strength.load(Ordering::Relaxed)).clamp(0.0, 1.0);
                if k < 0.999 {
                    for (o, d) in self.frame_out.iter_mut().zip(self.prev_in.iter()) {
                        *o = *o * k + *d * (1.0 - k);
                    }
                }
                p
            } else {
                self.frame_out.copy_from_slice(&self.frame_in);
                1.0
            };
            self.prev_in.copy_from_slice(&self.frame_in);
            let clarity = shared.clarity.load(Ordering::Relaxed);

            let mut sum = 0f32;
            for s in self.frame_out.iter_mut() {
                let v = self.hpf.run(*s / 32768.0);
                let shelved = self.shelf.run(v);
                *s = if clarity { shelved } else { v };
                sum += *s * *s;
            }
            let pow = sum / N as f32;
            let db = 10.0 * pow.max(1e-18).log10();
            shared.vad_level.store(db.to_bits(), Ordering::Relaxed);
            let voice = db > threshold && (!ns_on || vad_prob > 0.5);
            if voice {
                shared.last_voice_ms.store(shared.now_ms(), Ordering::Relaxed);
            }

            // Auto gain listens for speech on its own terms: soft talking up
            // to 12 dB under your sensitivity line still counts, so quiet,
            // breathy speech gets brought up instead of ignored.
            let speech_for_gain = if ns_on {
                vad_prob > 0.5 && db > -65.0
            } else {
                db > (threshold - 12.0).max(-60.0)
            };
            if agc_on {
                if speech_for_gain {
                    // React to the speaking level over ~70 ms, not to each
                    // 10 ms frame (single frames jump around a lot).
                    if self.speech_pow == 0.0 {
                        self.speech_pow = pow;
                    }
                    self.speech_pow += (pow - self.speech_pow) * 0.15;
                    let want = (TARGET_RMS / self.speech_pow.sqrt().max(1e-5)).clamp(MIN_GAIN, MAX_GAIN);
                    // Turn down fairly fast, up slowly (no pumping).
                    let rate = if want < self.gain { 0.1 } else { 0.02 };
                    self.gain += (want - self.gain) * rate;
                }
            } else {
                self.gain = 1.0;
            }
            shared.agc_gain.store(self.gain.to_bits(), Ordering::Relaxed);

            // Glide from the gain used at the end of the last frame to the
            // new one across this frame's 480 samples: no steps, no crackle.
            let from = self.applied;
            let to = self.gain * volume;
            self.applied = to;
            let mut sent = 0f32;
            for (i, s) in self.frame_out.iter().enumerate() {
                let g = from + (to - from) * (i + 1) as f32 / N as f32;
                let x = s * g;
                // The leveler only runs with auto gain (it's part of it).
                let x = if agc_on { x * self.comp.gain(x) } else { x };
                let need = if x.abs() > CEILING { CEILING / x.abs() } else { 1.0 };
                self.delay.push_back((x, need));
                // Look-ahead limiter: start turning down 2 ms before a peak
                // arrives, smoothly, then recover over ~70 ms.
                let target = self.delay.iter().fold(1.0f32, |m, d| m.min(d.1));
                if target < self.limit {
                    self.limit += (target - self.limit) * 0.06;
                } else {
                    self.limit += (1.0 - self.limit) * 0.0003;
                }
                if self.delay.len() > LOOKAHEAD {
                    let (y, _) = self.delay.pop_front().unwrap();
                    let y = soft_limit(y * self.limit);
                    sent += y * y;
                    out.push_back(y);
                }
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
    rs: Converter,
    conv: VecDeque<f32>,
    tmp: Vec<f32>,
    playing: bool,
    fade: f32,
    last: f32,
    fade_step: f32,
}

impl Playout {
    fn new(device_rate: u32) -> Self {
        Self {
            rs: Converter::new(RATE as u32, device_rate),
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
        self.rs.set_fast(backlog > OUT_CATCHUP);
        while self.playing && self.conv.len() < frames {
            let need = ((frames - self.conv.len()) as f64 * self.rs.step()).ceil() as usize + 1;
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

/// Stereo -> mono. Many headsets and virtual mics send the voice on one
/// channel and silence on the other; averaging those would halve your
/// volume, so if one channel stays 20 dB+ quieter we use only the live one.
struct Downmix {
    ch: usize,
    pow: Vec<f32>,
    /// None = average all channels; Some(i) = use channel i only.
    pick: Option<usize>,
    /// 0..1 crossfade from average to `pick` (so switching doesn't click).
    mix: f32,
    logged: bool,
}

impl Downmix {
    fn new(ch: usize) -> Self {
        Self { ch, pow: vec![0.0; ch], pick: None, mix: 0.0, logged: false }
    }

    fn run(&mut self, data: &[f32], mono: &mut Vec<f32>) {
        mono.clear();
        let ch = self.ch;
        if ch == 1 {
            mono.extend_from_slice(data);
            return;
        }
        let frames = (data.len() / ch).max(1);
        for c in 0..ch {
            let p = data.iter().skip(c).step_by(ch).map(|x| x * x).sum::<f32>() / frames as f32;
            self.pow[c] += (p - self.pow[c]) * 0.05;
        }
        let (loud, lp) = self
            .pow
            .iter()
            .enumerate()
            .fold((0, 0f32), |m, (i, p)| if *p > m.1 { (i, *p) } else { m });
        let others_quiet = self.pow.iter().enumerate().all(|(i, p)| i == loud || *p < lp * 0.01);
        if lp > 1e-7 {
            self.pick = if others_quiet { Some(loud) } else { None };
            if !self.logged && self.pick.is_some() {
                tracing::info!("mic: only channel {loud} has sound; using it alone");
                self.logged = true;
            }
        }
        let target = if self.pick.is_some() { 1.0 } else { 0.0 };
        let step = 1.0 / 4800.0; // 100 ms crossfade
        for frame in data.chunks(ch) {
            if self.mix < target {
                self.mix = (self.mix + step).min(1.0);
            } else if self.mix > target {
                self.mix = (self.mix - step).max(0.0);
            }
            let avg = frame.iter().sum::<f32>() / ch as f32;
            let one = frame[self.pick.unwrap_or(0).min(frame.len() - 1)];
            mono.push(avg * (1.0 - self.mix) + one * self.mix);
        }
    }
}

fn build_input(dev: &cpal::Device, shared: Arc<Shared>) -> Option<cpal::Stream> {
    let sup = dev.default_input_config().ok()?;
    tracing::info!("mic: {:?} {:?}", dev.description().ok(), sup);
    let fmt = sup.sample_format();
    let cfg = sup.config();
    let ch = cfg.channels.max(1) as usize;
    let mut rs = Converter::new(cfg.sample_rate, RATE as u32);
    let mut mono: Vec<f32> = Vec::new();
    let mut at48: Vec<f32> = Vec::new();
    let mut proc = MicProcessor::new();
    let mut downmix = Downmix::new(ch);
    let mut handle = move |data: &[f32]| {
        if data.iter().filter(|x| x.abs() >= 0.99).count() >= 3 {
            shared.last_clip_ms.store(shared.now_ms().max(1), Ordering::Relaxed);
        }
        downmix.run(data, &mut mono);
        let backlog = shared.mic.lock().len();
        rs.set_fast(backlog > MIC_CATCHUP);
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

    /// A voice getting louder and softer (like syllables) through auto gain
    /// and the limiter. The output must stay smooth: no steps in the
    /// waveform, which is what crackles like a loose jack.
    #[test]
    fn auto_gain_has_no_crackle() {
        let sh = Shared::new();
        sh.noise_suppression.store(false, Ordering::Relaxed);
        sh.input_volume.store(1.5f32.to_bits(), Ordering::Relaxed);
        let mut p = MicProcessor::new();
        let input: Vec<f32> = (0..RATE * 4)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let env = 0.02 + 0.3 * (0.5 + 0.5 * (t * 4.0 * 6.283).sin()).powi(2);
                env * (t * 180.0 * 6.283).sin()
            })
            .collect();
        let mut out = VecDeque::new();
        p.run(&input, &mut out, &sh);
        let out: Vec<f32> = out.into_iter().collect();
        let body = &out[RATE..];
        // Steps show up as spikes in the 2nd difference; a clean 180 Hz
        // tone at full scale has at most (2*pi*180/48000)^2 = 0.00055.
        let worst = body.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0f32, f32::max);
        let peak = body.iter().fold(0f32, |m, x| m.max(x.abs()));
        println!("agc: worst 2nd difference {worst:.5} (clean: <0.0006), peak {peak:.3}, gain {:.2}", p.gain);
        assert!(worst < 0.002, "crackle: waveform has steps");
        assert!(peak <= 1.0);
    }

    #[test]
    fn one_sided_stereo_is_not_halved() {
        let mut d = Downmix::new(2);
        let mut mono = Vec::new();
        let mut last = Vec::new();
        for k in 0..100 {
            let data: Vec<f32> = (0..480)
                .flat_map(|i| [((k * 480 + i) as f32 * 0.03).sin() * 0.5, 0.0])
                .collect();
            d.run(&data, &mut mono);
            last = mono.clone();
        }
        let peak = last.iter().fold(0f32, |m, x| m.max(x.abs()));
        println!("downmix: one-sided stereo peak {peak:.3} (input 0.5)");
        assert!(peak > 0.45);
    }

    /// A 44.1 kHz mic with an 8 kHz tone (the "s"/"t" region of speech):
    /// the sinc converter must keep it at full strength.
    #[test]
    fn resampling_keeps_crisp_highs() {
        let input: Vec<f32> = (0..44_100).map(|i| (i as f32 * 8000.0 * 6.283 / 44_100.0).sin() * 0.5).collect();
        let rms_of = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let mut hq = Converter::new(44_100, 48_000);
        let mut out_hq: Vec<f32> = Vec::new();
        for c in input.chunks(441) {
            hq.run(c, &mut out_hq);
        }
        let mut lin = Resampler::new(44_100, 48_000);
        let mut out_lin: Vec<f32> = Vec::new();
        lin.run(&input, &mut out_lin);
        let a = rms_of(&out_hq[4800..out_hq.len() - 4800]) / rms_of(&input);
        let b = rms_of(&out_lin[4800..out_lin.len() - 4800]) / rms_of(&input);
        println!("8 kHz kept: sinc {:.1}%  linear {:.1}%  ({} samples out for 1 s)", a * 100.0, b * 100.0, out_hq.len());
        assert!(a > 0.97);
        assert!((out_hq.len() as i64 - 48_000).abs() < 600);
    }

    #[test]
    fn high_pass_removes_rumble_keeps_voice() {
        let gain_at = |f: f32| {
            let mut h = Biquad::high_pass(80.0, 48_000.0);
            let out: Vec<f32> = (0..48_000).map(|i| h.run((i as f32 * f * 6.283 / 48_000.0).sin())).collect();
            (out[24_000..].iter().map(|x| x * x).sum::<f32>() / 24_000.0 * 2.0).sqrt()
        };
        let (rumble, voice) = (gain_at(25.0), gain_at(300.0));
        println!("high-pass: 25 Hz kept {:.0}%, 300 Hz kept {:.0}%", rumble * 100.0, voice * 100.0);
        assert!(rumble < 0.15 && voice > 0.95);
    }

    #[test]
    fn clarity_boosts_presence_only() {
        let gain_at = |f: f32| {
            let mut h = Biquad::high_shelf(3000.0, 4.0, 48_000.0);
            let out: Vec<f32> = (0..48_000).map(|i| h.run((i as f32 * f * 6.283 / 48_000.0).sin())).collect();
            20.0 * ((out[24_000..].iter().map(|x| x * x).sum::<f32>() / 24_000.0 * 2.0).sqrt()).log10()
        };
        let (low, high) = (gain_at(300.0), gain_at(6000.0));
        println!("clarity: 300 Hz {low:+.1} dB, 6 kHz {high:+.1} dB");
        assert!(low.abs() < 0.5 && (high - 4.0).abs() < 0.7);
    }

    /// Loud and soft syllables: the leveler should bring them closer
    /// together (so the voice is louder overall without squashed peaks).
    #[test]
    fn leveler_evens_out_syllables() {
        let mut c = Compressor::new(48_000.0);
        let mut level = |amp: f32| {
            let mut sum = 0f32;
            for i in 0..9_600 {
                let x = amp * (i as f32 * 200.0 * 6.283 / 48_000.0).sin();
                let y = x * c.gain(x);
                if i >= 4_800 {
                    sum += y * y;
                }
            }
            10.0 * (sum / 4_800.0).log10()
        };
        let soft = level(0.03);
        let loud = level(0.5);
        let in_gap = 20.0 * (0.5f32 / 0.03).log10();
        println!("leveler: input gap {in_gap:.1} dB -> output gap {:.1} dB", loud - soft);
        assert!(loud - soft < in_gap - 6.0);
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
