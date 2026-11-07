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
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const RATE: usize = 48_000;
const MS: usize = RATE / 1000;

/// Max audio waiting to be sent (anything older is dropped).
const MIC_MAX: usize = 60 * MS;
/// Max audio waiting to be played before we skip ahead.
const OUT_MAX: usize = 80 * MS;
/// What we skip ahead to when OUT_MAX is exceeded.
const OUT_TARGET: usize = 30 * MS;

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
            let drop = o.len() - OUT_TARGET;
            o.drain(..drop);
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
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend(input.iter().copied());
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
pub fn start(shared: Arc<Shared>, deafened: Arc<AtomicBool>) -> AudioHandle {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("audio".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let host = cpal::default_host();
            let input = host.default_input_device().and_then(|d| build_input(&d, shared.clone()));
            let output = host
                .default_output_device()
                .and_then(|d| build_output(&d, shared.clone(), deafened.clone()));
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
    let mut handle = move |data: &[f32]| {
        mono.clear();
        let mut sum = 0f32;
        for frame in data.chunks(ch) {
            let s = frame.iter().sum::<f32>() / ch as f32;
            sum += s * s;
            mono.push(s);
        }
        let rms = (sum / mono.len().max(1) as f32).sqrt();
        let db = 20.0 * rms.max(1e-9).log10();
        shared.mic_level.store(db.to_bits(), Ordering::Relaxed);
        if db > f32::from_bits(shared.vad_db.load(Ordering::Relaxed)) {
            shared.last_voice_ms.store(shared.now_ms(), Ordering::Relaxed);
        }
        let mut m = shared.mic.lock();
        rs.run(&mono, &mut *m);
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
    let mut rs = Resampler::new(RATE as u32, cfg.sample_rate);
    let mut conv: VecDeque<f32> = VecDeque::new();
    let mut tmp: Vec<f32> = Vec::new();
    let mut fill = move |out: &mut [f32]| {
        let frames = out.len() / ch;
        // Pull enough 48k audio to make `frames` device-rate samples.
        while conv.len() < frames {
            let need = ((frames - conv.len()) as f64 * rs.step).ceil() as usize + 1;
            tmp.clear();
            {
                let mut o = shared.out.lock();
                let n = need.min(o.len());
                tmp.extend(o.drain(..n));
            }
            if tmp.is_empty() {
                break;
            }
            rs.run(&tmp, &mut conv);
        }
        let deaf = deafened.load(Ordering::Relaxed);
        for frame in out.chunks_mut(ch) {
            let s = conv.pop_front().unwrap_or(0.0);
            let s = if deaf { 0.0 } else { s.clamp(-1.0, 1.0) };
            for x in frame.iter_mut() {
                *x = s;
            }
        }
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
