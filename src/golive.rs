//! Go Live: share your screen (and your PC's sound) in your voice channel.
//!
//! Nothing here runs or holds memory until you press Go Live. While live:
//! - a capture thread grabs the screen (only when it changes, at most the
//!   chosen frame rate), scales it and converts it for the encoder;
//! - an encode thread turns pictures into H.264 (OpenH264, on the CPU);
//! - a send thread spreads packets out evenly, resends ones viewers lost,
//!   lowers the bitrate if the network is dropping packets, and sends
//!   keyframes when a viewer joins or asks;
//! - an audio thread sends what your PC plays (minus tinycord itself).
//! Stopping ends all of them and frees their buffers.

use crate::capture::{CapErr, Capturer};
use crate::convert::{self, I420};
use crate::rtp::{self, Feedback, History, Packet, VideoPacketizer};
use crate::state::{self, state, Live, LiveCfg};
use async_trait::async_trait;
use serde_json::{json, Value};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::video_hook::{self, davey, MediaSender};
use songbird::{Config, ConnectionInfo, Driver};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const HEIGHTS: [u32; 5] = [480, 720, 1080, 1440, 0];
pub const FPS: [u32; 3] = [15, 30, 60];

/// Shortest time between keyframes sent because a viewer asked. Keyframes
/// are big; answering every request at once can flood the connection.
const KEYFRAME_GAP: Duration = Duration::from_millis(700);

/// The best quality your account or this server allows, whichever is
/// higher: (max picture height, 0 = any; max frames/s). Discord's limits:
/// no Nitro 720p 30fps; Nitro Basic 1080p 60; Nitro any size 60;
/// server boost level 1 720p 60, level 2+ 1080p 60.
pub fn allowed(premium: u8, boost: u8) -> (u32, u32) {
    let user = match premium {
        2 => (0, 60),
        1 | 3 => (1080, 60),
        _ => (1080, 60),
    };
    let server = match boost {
        0 => (720, 30),
        1 => (720, 60),
        _ => (1080, 60),
    };
    let h = if user.0 == 0 || server.0 == 0 { 0 } else { user.0.max(server.0) };
    (h, user.1.max(server.1))
}

pub fn height_ok(h: u32, max_h: u32) -> bool {
    max_h == 0 || (h != 0 && h <= max_h)
}

/// Keep settings within what's allowed.
pub fn clamp(mut cfg: LiveCfg, (max_h, max_fps): (u32, u32)) -> LiveCfg {
    if !height_ok(cfg.height, max_h) {
        cfg.height = max_h;
    }
    if !FPS.contains(&cfg.fps) {
        cfg.fps = 30;
    }
    cfg.quality = cfg.quality.min(QUALITY.len() as u8 - 1);
    cfg.fps = cfg.fps.min(max_fps);
    cfg
}

/// Quality choices: name and how much more bitrate than Standard.
pub const QUALITY: [(&str, f64); 4] = [("Standard", 1.0), ("High", 1.6), ("Ultra", 2.2), ("Max", 3.0)];

/// Bitrate for a picture size, frame rate and quality choice. Bitrate is
/// what makes 1080p look sharp: in tests, High (1.6x) gave about 5 dB
/// better picture than Standard, while encoder effort settings barely
/// mattered.
pub fn target_bitrate(w: u32, h: u32, fps: u32, quality: u8) -> u32 {
    let mult = QUALITY[(quality as usize).min(QUALITY.len() - 1)].1;
    (bitrate(w, h, fps) as f64 * mult).min(25_000_000.0) as u32
}

/// Standard-quality bits per second for a picture size and frame rate.
pub fn bitrate(w: u32, h: u32, fps: u32) -> u32 {
    let px = (w as f64 * h as f64).max(1.0);
    // ~2.5 Mbit/s for 720p30, growing a bit slower than the pixel count.
    let at30 = 2_500_000.0 * (px / (1280.0 * 720.0)).powf(0.75);
    let f = match fps {
        0..=15 => 0.65,
        16..=30 => 1.0,
        _ => 1.5,
    };
    (at30 * f).clamp(600_000.0, 12_000_000.0) as u32
}

// ---------------------------------------------------------------- session

struct Session {
    key: String,
    guild: u64,
    channel: u64,
    cfg: LiveCfg,
    server_id: Option<u64>,
    server: Option<(String, String)>, // (endpoint, token)
    driver: Option<Driver>,
    connected_with: Option<(String, String)>,
    pipeline: Option<Pipeline>,
    cont: Option<Arc<Continuity>>,
}

/// Packet numbering and the stream clock. These must carry on when
/// settings change mid-stream: starting them over makes viewers' players
/// treat every new frame as old and drop it (the stream freezes).
struct Continuity {
    t0: Instant,
    video_seq: Mutex<Option<u16>>,
    /// Next audio sequence number and timestamp, and when the last went out.
    audio: Mutex<Option<(u16, u32, Instant)>>,
}

impl Continuity {
    fn new() -> Arc<Self> {
        Arc::new(Self { t0: Instant::now(), video_seq: Mutex::new(None), audio: Mutex::new(None) })
    }
}

static SESSION: tokio::sync::Mutex<Option<Session>> = tokio::sync::Mutex::const_new(None);

fn id(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()).unwrap_or(0)
}

fn set_status(s: impl Into<String>) {
    let s = s.into();
    tracing::info!("go live: {s}");
    if let Some(l) = state().live.as_mut() {
        l.status = s;
    }
    state::changed();
}

pub async fn current_key() -> Option<String> {
    SESSION.lock().await.as_ref().map(|s| s.key.clone())
}

/// Same channel as the current stream?
pub async fn live_in(guild: u64, channel: u64) -> bool {
    SESSION.lock().await.as_ref().is_some_and(|s| s.guild == guild && s.channel == channel)
}

/// Start a stream (the caller sends the gateway messages).
pub async fn begin(key: String, guild: u64, channel: u64, cfg: LiveCfg) {
    stop().await;
    tracing::info!("going live {key} {cfg:?}");
    state().live = Some(Live { guild, channel, status: "Starting stream...".into(), ..Default::default() });
    state::changed();
    *SESSION.lock().await = Some(Session {
        key,
        guild,
        channel,
        cfg,
        server_id: None,
        server: None,
        driver: None,
        connected_with: None,
        pipeline: None,
        cont: None,
    });
}

/// New settings while live: restart the picture with them.
pub async fn reconfigure(cfg: LiveCfg) {
    let mut guard = SESSION.lock().await;
    let Some(s) = guard.as_mut() else { return };
    s.cfg = cfg;
    if let Some(p) = s.pipeline.take() {
        let sender = p.sender.clone();
        let _ = tokio::task::spawn_blocking(move || p.finish(false)).await;
        let cont = s.cont.get_or_insert_with(Continuity::new).clone();
        s.pipeline = Some(Pipeline::start(sender, cfg, cont));
    }
}

/// End the stream and free everything. Returns the stream key if one was on
/// (the caller tells Discord).
pub async fn stop() -> Option<String> {
    let old = SESSION.lock().await.take();
    let key = if let Some(mut s) = old {
        if let Some(p) = s.pipeline.take() {
            let _ = tokio::task::spawn_blocking(move || p.finish(true)).await;
        }
        if let Some(mut d) = s.driver.take() {
            d.leave();
        }
        tracing::info!("stream {} ended", s.key);
        Some(s.key)
    } else {
        None
    };
    if let Ok(mut g) = video_hook::SENDER.write() {
        *g = None;
    }
    if let Ok(mut g) = video_hook::RTCP_SINK.write() {
        *g = None;
    }
    if let Ok(mut g) = video_hook::WS_TAP_SEND.write() {
        *g = None;
    }
    if state().live.take().is_some() {
        state::changed();
    }
    key
}

pub async fn on_event(t: &str, d: &Value, session_id: Option<String>) {
    let key = d["stream_key"].as_str().unwrap_or("");
    let mut guard = SESSION.lock().await;
    let Some(s) = guard.as_mut() else { return };
    if key != s.key {
        return;
    }
    match t {
        "STREAM_CREATE" => s.server_id = Some(id(&d["rtc_server_id"])).filter(|x| *x != 0),
        "STREAM_SERVER_UPDATE" => {
            if let (Some(ep), Some(tok)) = (d["endpoint"].as_str(), d["token"].as_str()) {
                s.server = Some((ep.to_string(), tok.to_string()));
            }
        }
        "STREAM_UPDATE" => {
            let n = d["viewer_ids"].as_array().map(|a| a.len()).unwrap_or(0);
            if let Some(l) = state().live.as_mut() {
                l.viewers = n;
            }
            state::changed();
            return;
        }
        "STREAM_DELETE" => {
            let reason = d["reason"].as_str().unwrap_or("ended").to_string();
            drop(guard);
            stop().await;
            if reason != "user_requested" {
                state::set_status(format!("Your stream ended ({reason})"));
            }
            return;
        }
        _ => return,
    }
    try_connect(s, session_id).await;
}

async fn try_connect(s: &mut Session, session_id: Option<String>) {
    let (Some(server_id), Some((endpoint, token)), Some(sid)) = (s.server_id, s.server.clone(), session_id) else {
        return;
    };
    let key = (endpoint.clone(), token.clone());
    if s.connected_with.as_ref() == Some(&key) {
        return;
    }
    let me = state().me;
    let (Some(g), Some(c), Some(u)) =
        (NonZeroU64::new(server_id), NonZeroU64::new(server_id - 1), NonZeroU64::new(me))
    else {
        return;
    };
    let info = ConnectionInfo {
        channel_id: ChannelId::from(c),
        endpoint,
        guild_id: GuildId::from(g),
        session_id: sid,
        token,
        user_id: UserId::from(u),
    };
    // A new server: stop sending to the old one first.
    if let Some(p) = s.pipeline.take() {
        let _ = tokio::task::spawn_blocking(move || p.finish(true)).await;
    }
    if let Ok(mut g) = video_hook::SENDER.write() {
        *g = None;
    }
    if let Ok(mut g) = video_hook::WS_TAP_SEND.write() {
        *g = Some(on_ws);
    }
    let driver = s.driver.get_or_insert_with(|| {
        let mut d = Driver::new(Config::default().video_send(true));
        for ev in [CoreEvent::DriverConnect, CoreEvent::DriverReconnect, CoreEvent::DriverDisconnect] {
            d.add_global_event(Event::Core(ev), LiveEvents);
        }
        d
    });
    set_status("Connecting...");
    if let Err(e) = driver.connect(info).await {
        set_status(format!("Couldn't connect: {e}"));
        return;
    }
    let sender = video_hook::SENDER.read().ok().and_then(|g| g.clone());
    let Some(sender) = sender else {
        set_status("Couldn't start sending");
        return;
    };
    s.connected_with = Some(key);
    tracing::info!(
        "stream connection up: audio {} video {} rtx {}",
        sender.audio_ssrc,
        sender.video_ssrc,
        sender.rtx_ssrc
    );
    // New connection: numbering starts fresh.
    let cont = Continuity::new();
    s.cont = Some(cont.clone());
    s.pipeline = Some(Pipeline::start(sender, s.cfg, cont));
}

/// Messages on the stream's voice connection (just logged).
fn on_ws(text: &str) {
    if text.contains("\"op\":15") || text.contains("\"op\": 15") {
        tracing::info!("stream sink wants: {}", &text[..text.len().min(200)]);
    }
}

struct LiveEvents;

#[async_trait]
impl EventHandler for LiveEvents {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::DriverConnect(_) => tracing::info!("go live connection up"),
            EventContext::DriverReconnect(_) => tracing::info!("go live connection resumed"),
            EventContext::DriverDisconnect(d) => {
                tracing::warn!("go live connection dropped: {:?}", d.reason);
                if d.reason.is_some() {
                    set_status("Connection to the stream server dropped. Press Go Live to try again.");
                    state::send(state::Cmd::StopLive);
                }
            }
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------- pipeline

/// Shared between the stream's threads.
struct Shared {
    stop: AtomicBool,
    video_ssrc: u32,
    /// A viewer asked for a keyframe.
    keyframe: AtomicBool,
    nacks: Mutex<Vec<u16>>,
    /// Encoder bitrate the send thread wants (lowered when packets get lost).
    bitrate: AtomicU32,
    fps: u32,
    quality: u8,
    /// Last picture size (w << 16 | h).
    size: AtomicU32,
    frames: AtomicU32,
    bytes: AtomicU64,
    loss: Mutex<Vec<u8>>,
    remb: AtomicU64,
    st: Stats,
}

/// Counters for the log (reset every few seconds).
#[derive(Default)]
struct Stats {
    captured: AtomicU32,
    encoded: AtomicU32,
    keyframes: AtomicU32,
    rc_skipped: AtomicU32,
    e2ee_wait: AtomicU32,
    encode_us: AtomicU64,
    encode_us_max: AtomicU64,
    capture_us_max: AtomicU64,
    asked_keyframe: AtomicU32,
    nacked: AtomicU32,
    resent: AtomicU32,
    resend_missing: AtomicU32,
    send_retry: AtomicU32,
    send_err: AtomicU32,
    queue_max: AtomicU32,
    e2ee_failed: AtomicU32,
    loss_reports: AtomicU32,
    /// Worst share of packets lost in one report (0-255).
    loss_max: AtomicU32,
}

fn take(a: &AtomicU32) -> u32 {
    a.swap(0, Ordering::Relaxed)
}

fn take64(a: &AtomicU64) -> u64 {
    a.swap(0, Ordering::Relaxed)
}

static FEEDBACK: RwLock<Option<Arc<Shared>>> = RwLock::new(None);

fn on_rtcp(buf: &[u8]) {
    let Some(sh) = FEEDBACK.read().ok().and_then(|g| g.clone()) else { return };
    for fb in rtp::parse_feedback(buf, sh.video_ssrc) {
        match fb {
            Feedback::Keyframe => {
                sh.st.asked_keyframe.fetch_add(1, Ordering::Relaxed);
                sh.keyframe.store(true, Ordering::Relaxed);
            }
            Feedback::Nack(seqs) => {
                sh.st.nacked.fetch_add(seqs.len() as u32, Ordering::Relaxed);
                sh.nacks.lock().unwrap().extend(seqs);
            }
            Feedback::Loss(f) => {
                sh.st.loss_reports.fetch_add(1, Ordering::Relaxed);
                sh.st.loss_max.fetch_max(f as u32, Ordering::Relaxed);
                sh.loss.lock().unwrap().push(f);
            }
            Feedback::Remb(b) => sh.remb.store(b, Ordering::Relaxed),
        }
    }
}

/// Holds the newest item; a newer one replaces it (so a slow encoder skips
/// frames instead of falling behind).
struct Slot<T> {
    item: Mutex<Option<T>>,
    cv: Condvar,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self { item: Mutex::new(None), cv: Condvar::new() }
    }
    fn put(&self, v: T) -> Option<T> {
        let old = self.item.lock().unwrap().replace(v);
        self.cv.notify_one();
        old
    }
    fn take(&self, timeout: Duration) -> Option<T> {
        let g = self.item.lock().unwrap();
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |x| x.is_none()).unwrap();
        g.take()
    }
}

struct Frame {
    pic: I420,
    at: Instant,
}

struct Encoded {
    packets: Vec<Packet>,
    ts: u32,
    at: Instant,
}

pub struct Pipeline {
    sh: Arc<Shared>,
    sender: Arc<MediaSender>,
    threads: Vec<JoinHandle<()>>,
}

impl Pipeline {
    fn start(sender: Arc<MediaSender>, cfg: LiveCfg, cont: Arc<Continuity>) -> Pipeline {
        // The server normally hands out the video SSRCs; fall back to the
        // usual layout (audio + 1, + 2) if it didn't.
        let video_ssrc = if sender.video_ssrc != 0 { sender.video_ssrc } else { sender.audio_ssrc.wrapping_add(1) };
        let rtx_ssrc = if sender.video_ssrc != 0 { sender.rtx_ssrc } else { sender.audio_ssrc.wrapping_add(2) };
        let sh = Arc::new(Shared {
            stop: AtomicBool::new(false),
            video_ssrc,
            keyframe: AtomicBool::new(true),
            nacks: Mutex::new(Vec::new()),
            bitrate: AtomicU32::new(0),
            fps: cfg.fps,
            quality: cfg.quality,
            size: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            loss: Mutex::new(Vec::new()),
            remb: AtomicU64::new(0),
            st: Stats::default(),
        });
        *FEEDBACK.write().unwrap() = Some(sh.clone());
        if let Ok(mut g) = video_hook::RTCP_SINK.write() {
            *g = Some(on_rtcp);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }

        // Tell the server what's coming; the size is what the settings ask
        // for on the chosen screen.
        let mons = crate::capture::monitors();
        let (sw, shh) = mons
            .get(crate::capture::resolve(cfg.monitor, &mons))
            .map(|m| (m.w as usize, m.h as usize))
            .unwrap_or((1920, 1080));
        let (w, h) = convert::target_size(sw, shh, cfg.height as usize);
        let video = json!({"op": 12, "d": {
            "audio_ssrc": sender.audio_ssrc,
            "video_ssrc": video_ssrc,
            "rtx_ssrc": rtx_ssrc,
            "streams": [{
                "type": "video", "rid": "100", "ssrc": video_ssrc, "active": true, "quality": 100,
                "rtx_ssrc": rtx_ssrc, "max_bitrate": target_bitrate(w as u32, h as u32, cfg.fps, cfg.quality) * 2,
                "max_framerate": cfg.fps,
                "max_resolution": {"type": "fixed", "width": w, "height": h}
            }]
        }});
        sender.send_ws(video.to_string());
        sender.send_ws(json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": sender.audio_ssrc}}).to_string());

        let slot = Arc::new(Slot::<Job>::new());
        let (free_tx, free_rx) = mpsc::channel::<I420>();
        let (enc_tx, enc_rx) = mpsc::channel::<Encoded>();
        let mut threads = Vec::new();
        {
            let (sh, slot) = (sh.clone(), slot.clone());
            threads.push(spawn("tinycord-capture", move || capture_thread(cfg, sh, slot, free_rx)));
        }
        {
            let (sh, sender) = (sh.clone(), sender.clone());
            let cont = cont.clone();
            threads.push(spawn("tinycord-encode", move || encode_thread(cfg, sh, sender, slot, free_tx, enc_tx, cont)));
        }
        {
            let (sh, sender) = (sh.clone(), sender.clone());
            threads.push(spawn("tinycord-send", move || send_thread(sh, sender, enc_rx, rtx_ssrc)));
        }
        if cfg.sound {
            let (sh, sender) = (sh.clone(), sender.clone());
            let cont = cont.clone();
            threads.push(spawn("tinycord-pcsound", move || audio_thread(sh, sender, cont)));
        }
        set_status("Live");
        Pipeline { sh, sender, threads }
    }

    /// Stop all threads and wait for them (well under a second). `end` =
    /// the stream is ending (not just restarting with new settings).
    fn finish(self, end: bool) {
        self.sh.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        let mut fb = FEEDBACK.write().unwrap();
        if fb.as_ref().is_some_and(|x| Arc::ptr_eq(x, &self.sh)) {
            *fb = None;
        }
        drop(fb);
        if end {
            // Tell viewers the picture stopped.
            let off = json!({"op": 12, "d": {"audio_ssrc": self.sender.audio_ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []}});
            self.sender.send_ws(off.to_string());
            self.sender
                .send_ws(json!({"op": 5, "d": {"speaking": 0, "delay": 0, "ssrc": self.sender.audio_ssrc}}).to_string());
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("thread")
}

fn live_mut(f: impl FnOnce(&mut Live)) {
    if let Some(l) = state().live.as_mut() {
        f(l);
    }
    state::changed();
}

// ---------------------------------------------------------------- capture

/// What the capture clock hands the encoder on each tick.
enum Job {
    /// A new picture.
    New(Frame),
    /// Nothing changed: send the last picture again, stamped with this tick.
    Repeat(Instant),
}

/// Hand a job to the encoder. A new picture that's still waiting is never
/// replaced by a repeat (only by a newer picture, which is returned here so
/// its buffer can be reused).
fn post(slot: &Slot<Job>, job: Job) -> Option<Frame> {
    let mut g = slot.item.lock().unwrap();
    if matches!((&*g, &job), (Some(Job::New(_)), Job::Repeat(_))) {
        return None;
    }
    let old = g.replace(job);
    drop(g);
    slot.cv.notify_one();
    match old {
        Some(Job::New(f)) => Some(f),
        _ => None,
    }
}

/// The stream's clock: exactly `fps` ticks a second. On each tick, take the
/// newest screen picture if anything changed (Windows collects all changes
/// since the last look), otherwise ask for a repeat. Viewers get a steady
/// frame rate whatever is on screen.
fn capture_thread(cfg: LiveCfg, sh: Arc<Shared>, slot: Arc<Slot<Job>>, free: mpsc::Receiver<I420>) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::*;
        // Real pixels, whatever the display scaling.
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let interval = Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64);
    let mut cap: Option<Capturer> = None;
    let mut retry_at = Instant::now();
    let mut tick = Instant::now();
    let mut scaled = Vec::new();
    let mut spare: Vec<I420> = Vec::new();
    let mut failed_note = false;
    while !sh.stop.load(Ordering::Relaxed) {
        tick += interval;
        let now = Instant::now();
        if tick + interval < now {
            tick = now; // fell far behind (PC busy): restart the clock
        } else if tick > now {
            std::thread::sleep(tick - now);
        }

        if cap.is_none() && Instant::now() >= retry_at {
            match Capturer::new(cfg.monitor) {
                Ok(c) => {
                    cap = Some(c);
                    if failed_note {
                        set_status("Live");
                        failed_note = false;
                    }
                }
                Err(CapErr::Lost) => retry_at = Instant::now() + Duration::from_millis(300),
                Err(CapErr::Failed(msg)) => {
                    set_status(format!("Screen capture: {msg}"));
                    failed_note = true;
                    retry_at = Instant::now() + Duration::from_secs(3);
                }
            }
        }
        let changed = match cap.as_mut().map(|c| c.next(0)) {
            Some(Ok(changed)) => changed,
            Some(Err(_)) => {
                cap = None; // screen went away for a moment; viewers keep the last picture
                false
            }
            None => false,
        };
        // A new picture, or an earlier copy that may be back from the GPU now.
        let want = changed || cap.as_ref().is_some_and(|c| c.has_pending());
        let Some(c) = cap.as_mut().filter(|_| want) else {
            post(&slot, Job::Repeat(tick));
            continue;
        };
        let t = Instant::now();
        while let Ok(p) = free.try_recv() {
            spare.push(p);
        }
        let mut pic = spare.pop().unwrap_or(I420 { w: 0, h: 0, data: Vec::new() });
        let done = c.with_frame(|src, w, h, pitch, ptr| {
            let (dw, dh) = convert::target_size(w, h, cfg.height as usize);
            if (dw, dh) == (w, h) {
                convert::bgra_to_i420(src, w, h, pitch, &mut pic);
                if let Some((p, px, py)) = ptr {
                    convert::pointer_into_i420(src, pitch, &mut pic, p, px, py, 1.0, 1.0);
                }
            } else {
                convert::scale_bgra(src, w, h, pitch, dw, dh, &mut scaled);
                convert::bgra_to_i420(&scaled, dw, dh, dw * 4, &mut pic);
                if let Some((p, px, py)) = ptr {
                    let (sx, sy) = (dw as f32 / w as f32, dh as f32 / h as f32);
                    convert::pointer_into_i420(&scaled, dw * 4, &mut pic, p, px, py, sx, sy);
                }
            }
        });
        let us = t.elapsed().as_micros() as u64;
        sh.st.capture_us_max.fetch_max(us, Ordering::Relaxed);
        if done.is_some() {
            sh.st.captured.fetch_add(1, Ordering::Relaxed);
            if let Some(old) = post(&slot, Job::New(Frame { pic, at: tick })) {
                spare.push(old.pic);
            }
        } else {
            spare.push(pic);
            post(&slot, Job::Repeat(tick));
        }
        spare.truncate(2);
    }
}

// ---------------------------------------------------------------- encode

fn encoder_config(fps: u32, bitrate: u32) -> openh264::encoder::EncoderConfig {
    use openh264::encoder::*;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8) / 2;
    EncoderConfig::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        // Never drop frames to save bitrate (that's what made the frame rate
        // jump around); the encoder lowers quality a little instead.
        .skip_frames(false)
        .complexity(Complexity::Low)
        .num_threads(threads as u16)
        // A few slices let the encoder use several cores. Not more: the
        // end-to-end encryption can only describe a few dozen pieces per
        // frame, and frames with too many can't be sent at all.
        .slice_count(threads as u32)
        .intra_frame_period(IntraFramePeriod::auto())
        .vui(VuiConfig::bt709())
}

#[allow(clippy::too_many_arguments)]
fn encode_thread(
    cfg: LiveCfg,
    sh: Arc<Shared>,
    sender: Arc<MediaSender>,
    slot: Arc<Slot<Job>>,
    free: mpsc::Sender<I420>,
    out: mpsc::Sender<Encoded>,
    cont: Arc<Continuity>,
) {
    let t0 = cont.t0;
    use openh264::encoder::{Encoder, FrameType};
    use openh264::formats::YUVSlices;
    use openh264::{OpenH264API, Timestamp};

    let mut enc: Option<(Encoder, usize, usize, u32)> = None;
    let mut last: Option<Frame> = None;
    let mut last_idr = Instant::now() - Duration::from_secs(60);
    let mut need_idr = true;
    let mut pk = match cont.video_seq.lock().unwrap().take() {
        Some(seq) => VideoPacketizer::with_seq(sh.video_ssrc, seq),
        None => VideoPacketizer::new(sh.video_ssrc),
    };
    let mut last_at = t0;
    // The capture clock sends a job every tick; this only matters if it
    // stalls (then keep the stream alive with repeats).
    let stall = Duration::from_millis(200);
    while !sh.stop.load(Ordering::Relaxed) {
        let at = match slot.take(stall) {
            Some(Job::New(f)) => {
                let at = f.at;
                if let Some(old) = last.replace(f) {
                    let _ = free.send(old.pic);
                }
                at
            }
            Some(Job::Repeat(t)) if last.is_some() => t,
            None if last.is_some() => Instant::now(),
            _ => continue,
        };
        // Timestamps must only go forward.
        let at = at.max(last_at + Duration::from_millis(1));
        last_at = at;
        let pic = &last.as_ref().unwrap().pic;
        let (w, h) = (pic.w, pic.h);
        if w == 0 || h == 0 {
            continue;
        }
        let size = ((w as u32) << 16) | h as u32;
        if sh.size.swap(size, Ordering::Relaxed) != size {
            // New picture size: start from that size's normal bitrate.
            sh.bitrate.store(target_bitrate(w as u32, h as u32, cfg.fps, cfg.quality), Ordering::Relaxed);
        }
        let want = sh.bitrate.load(Ordering::Relaxed);
        let rebuild = match &enc {
            None => true,
            Some((_, ew, eh, eb)) => *ew != w || *eh != h || want.abs_diff(*eb) > *eb / 10,
        };
        if rebuild {
            match Encoder::with_api_config(OpenH264API::from_source(), encoder_config(cfg.fps, want)) {
                Ok(e) => {
                    tracing::info!("encoder {w}x{h} {}fps {} kbit/s", cfg.fps, want / 1000);
                    enc = Some((e, w, h, want));
                    need_idr = true;
                }
                Err(e) => {
                    tracing::error!("encoder: {e}");
                    set_status("Video encoder failed to start");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        let (e, ..) = enc.as_mut().unwrap();
        // A viewer asked for a keyframe. If one went out very recently,
        // keep the request and answer it a moment later (never drop it).
        if last_idr.elapsed() >= KEYFRAME_GAP && sh.keyframe.swap(false, Ordering::Relaxed) {
            need_idr = true;
        }
        // Safety net in case a request got lost.
        if last_idr.elapsed() >= Duration::from_secs(20) {
            need_idr = true;
        }
        if need_idr {
            e.force_intra_frame();
        }
        let (y, rest) = pic.data.split_at(w * h);
        let (u, v) = rest.split_at((w / 2) * (h / 2));
        let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
        let ms = at.saturating_duration_since(t0).as_millis() as u64;
        let enc_start = Instant::now();
        let res = e.encode_at(&src, Timestamp::from_millis(ms)).map(|bs| (bs.to_vec(), bs.frame_type()));
        let us = enc_start.elapsed().as_micros() as u64;
        sh.st.encode_us.fetch_add(us, Ordering::Relaxed);
        sh.st.encode_us_max.fetch_max(us, Ordering::Relaxed);
        let (au, kind) = match res {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!("encode: {err}");
                enc = None;
                continue;
            }
        };
        if au.is_empty() || matches!(kind, FrameType::Skip | FrameType::Invalid) {
            sh.st.rc_skipped.fetch_add(1, Ordering::Relaxed);
            continue; // rate control skipped this one
        }
        sh.st.encoded.fetch_add(1, Ordering::Relaxed);
        if matches!(kind, FrameType::IDR | FrameType::I) {
            sh.st.keyframes.fetch_add(1, Ordering::Relaxed);
            need_idr = false;
            last_idr = Instant::now();
        }
        // End-to-end encryption (DAVE). Until it's ready nothing can go out;
        // the first picture after that must be a keyframe.
        let Some(frame) = sender.e2ee(davey::MediaType::VIDEO, davey::Codec::H264, &au) else {
            if sender.e2ee_ready() {
                // Ready but this frame couldn't be encrypted.
                sh.st.e2ee_failed.fetch_add(1, Ordering::Relaxed);
            } else {
                sh.st.e2ee_wait.fetch_add(1, Ordering::Relaxed);
            }
            need_idr = true;
            continue;
        };
        let ts = (at.saturating_duration_since(t0).as_micros() as u64 * 9 / 100) as u32;
        let packets = pk.packetize(&frame, ts);
        if out.send(Encoded { packets, ts, at }).is_err() {
            break;
        }
    }
    // Settings changed or stream ending: the next encoder carries on from here.
    *cont.video_seq.lock().unwrap() = Some(pk.next_seq());
}

// ---------------------------------------------------------------- send

fn send_thread(sh: Arc<Shared>, sender: Arc<MediaSender>, rx: mpsc::Receiver<Encoded>, rtx_ssrc: u32) {
    use std::collections::VecDeque;
    let mut queue: VecDeque<Packet> = VecDeque::new();
    let mut hist = History::new(2048);
    let mut allowance: f64 = 0.0;
    let mut clock = Instant::now();
    let (mut packets, mut octets) = (0u32, 0u32);
    let mut last_ts: Option<(u32, Instant)> = None;
    let mut last_sr = Instant::now();
    let mut last_stats = Instant::now();
    let mut stat_bytes = 0u64;
    let mut stat_frames = 0u32;
    // Bitrate control from viewers' loss reports.
    let mut loss_avg = 0.0f64;
    let mut last_change = Instant::now();
    let mut clean_since = Instant::now();

    let mut last_log = Instant::now();
    // Ok(bytes), or Err(()) when the OS send buffer is full (try again).
    let send = |p: &Packet| -> Result<usize, ()> {
        match sender.send(&p.header, &p.body) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                sh.st.send_retry.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
            Err(e) => {
                tracing::debug!("send: {e}");
                sh.st.send_err.fetch_add(1, Ordering::Relaxed);
                Ok(0)
            }
        }
    };

    while !sh.stop.load(Ordering::Relaxed) {
        let wait = if queue.is_empty() { Duration::from_millis(20) } else { Duration::ZERO };
        let got = if wait.is_zero() { rx.try_recv().ok() } else { rx.recv_timeout(wait).ok() };
        if let Some(f) = got {
            last_ts = Some((f.ts, f.at));
            stat_frames += 1;
            queue.extend(f.packets);
            sh.st.queue_max.fetch_max(queue.len() as u32, Ordering::Relaxed);
            // Way behind (shouldn't happen): start over from a keyframe.
            if queue.len() > 4000 {
                queue.clear();
                sh.keyframe.store(true, Ordering::Relaxed);
            }
        }

        // Resend what viewers lost (before new packets).
        let nacks = std::mem::take(&mut *sh.nacks.lock().unwrap());
        for seq in nacks {
            let p = if rtx_ssrc != 0 { hist.rtx(seq, rtx_ssrc) } else { hist.get(seq) };
            match p {
                Some(p) => {
                    sh.st.resent.fetch_add(1, Ordering::Relaxed);
                    allowance -= send(&p).unwrap_or(0) as f64;
                }
                None => {
                    sh.st.resend_missing.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Pace at 2.5x the video bitrate so a big keyframe doesn't hit the
        // network as one burst.
        let rate = (sh.bitrate.load(Ordering::Relaxed) as f64 * 2.5).max(5_000_000.0) / 8.0;
        let now = Instant::now();
        allowance = (allowance + now.duration_since(clock).as_secs_f64() * rate).min(12_000.0);
        clock = now;
        while allowance > 0.0 {
            let Some(p) = queue.pop_front() else { break };
            let Ok(n) = send(&p) else {
                queue.push_front(p); // buffer full: same packet next round
                break;
            };
            allowance -= n as f64;
            packets = packets.wrapping_add(1);
            octets = octets.wrapping_add(p.body.len() as u32);
            stat_bytes += n as u64;
            hist.push(&p);
        }
        if !queue.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }

        // Sender report once a second (lets viewers line up sound and picture).
        if last_sr.elapsed() >= Duration::from_secs(1) {
            last_sr = Instant::now();
            if let Some((ts, at)) = last_ts {
                let rtp_ts = ts.wrapping_add((at.elapsed().as_micros() as u64 * 9 / 100) as u32);
                let (h, b) = rtp::sender_report(sh.video_ssrc, rtp::ntp_now(), rtp_ts, packets, octets);
                let _ = sender.send(&h, &b);
            }
            adapt(&sh, &mut loss_avg, &mut last_change, &mut clean_since);
        }

        if last_log.elapsed() >= Duration::from_secs(5) {
            let secs = last_log.elapsed().as_secs_f64();
            last_log = Instant::now();
            log_stats(&sh, secs);
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            let secs = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();
            let size = sh.size.load(Ordering::Relaxed);
            let kbps = (stat_bytes as f64 * 8.0 / secs / 1000.0) as u32;
            let fps = (stat_frames as f64 / secs).round() as u32;
            sh.bytes.fetch_add(stat_bytes, Ordering::Relaxed);
            sh.frames.fetch_add(stat_frames, Ordering::Relaxed);
            stat_bytes = 0;
            stat_frames = 0;
            live_mut(|l| {
                l.w = size >> 16;
                l.h = size & 0xffff;
                l.kbps = kbps;
                l.fps = fps;
            });
        }
    }
}

/// One line in the log every 5 s: what the stream is doing on this PC.
fn log_stats(sh: &Shared, secs: f64) {
    let st = &sh.st;
    let encoded = take(&st.encoded);
    let enc_us = take64(&st.encode_us);
    let size = sh.size.load(Ordering::Relaxed);
    tracing::info!(
        "live {}x{}: capture {:.0} fps (max {:.1} ms), encode {:.0} fps (avg {:.1} ms, max {:.1} ms), \
         rc skipped {}, keyframes {} (viewers asked {}), lost {} resent {} too old {}, \
         send retries {} errors {}, queue max {}, waiting for e2ee {} failed {}, target {} kbit/s, \
         loss reports {} (worst {}%), server estimate {} kbit/s",
        size >> 16,
        size & 0xffff,
        take(&st.captured) as f64 / secs,
        take64(&st.capture_us_max) as f64 / 1000.0,
        encoded as f64 / secs,
        if encoded > 0 { enc_us as f64 / encoded as f64 / 1000.0 } else { 0.0 },
        take64(&st.encode_us_max) as f64 / 1000.0,
        take(&st.rc_skipped),
        take(&st.keyframes),
        take(&st.asked_keyframe),
        take(&st.nacked),
        take(&st.resent),
        take(&st.resend_missing),
        take(&st.send_retry),
        take(&st.send_err),
        take(&st.queue_max),
        take(&st.e2ee_wait),
        take(&st.e2ee_failed),
        sh.bitrate.load(Ordering::Relaxed) / 1000,
        take(&st.loss_reports),
        take(&st.loss_max) * 100 / 256,
        sh.remb.load(Ordering::Relaxed) / 1000,
    );
}

/// Lower the bitrate when viewers' reports show lost packets; creep back up
/// when the network is clean again. Each change costs one keyframe, so
/// changes are rare.
fn adapt(sh: &Shared, loss_avg: &mut f64, last_change: &mut Instant, clean_since: &mut Instant) {
    let reports = std::mem::take(&mut *sh.loss.lock().unwrap());
    for f in reports {
        *loss_avg = *loss_avg * 0.7 + f as f64 / 256.0 * 0.3;
    }
    let cur = sh.bitrate.load(Ordering::Relaxed);
    if cur == 0 {
        return;
    }
    let size = sh.size.load(Ordering::Relaxed);
    if size == 0 {
        return;
    }
    let full = target_bitrate(size >> 16, size & 0xffff, sh.fps, sh.quality);
    let remb = sh.remb.load(Ordering::Relaxed);
    let mut want = cur;
    if *loss_avg > 0.10 {
        *clean_since = Instant::now();
        want = (cur as f64 * 0.75) as u32;
    } else if *loss_avg > 0.03 {
        *clean_since = Instant::now();
    } else if clean_since.elapsed() > Duration::from_secs(20) {
        want = ((cur as f64 * 1.15) as u32).min(full);
    }
    // The server's bandwidth estimate only counts when packets are really
    // being lost; on its own it can sit far below what the connection does.
    if *loss_avg > 0.02 && remb > 100_000 && (want as u64) > remb * 9 / 10 {
        want = (remb * 9 / 10) as u32;
    }
    let floor = (full / 3).max(500_000);
    want = want.clamp(floor, full.max(floor));
    if want.abs_diff(cur) > cur / 10 && last_change.elapsed() > Duration::from_secs(8) {
        tracing::info!("bitrate {} -> {} kbit/s (loss {:.1}%)", cur / 1000, want / 1000, *loss_avg * 100.0);
        sh.bitrate.store(want, Ordering::Relaxed);
        *last_change = Instant::now();
        *clean_since = Instant::now();
    }
}

// ---------------------------------------------------------------- PC sound

fn audio_thread(sh: Arc<Shared>, sender: Arc<MediaSender>, cont: Arc<Continuity>) {
    use songbird::driver::opus::{Application, Bitrate, Channels, Encoder};
    let mut enc = match Encoder::new(48000, Channels::Stereo, Application::Audio) {
        Ok(e) => e,
        Err(e) => {
            live_mut(|l| l.sound_note = format!("PC sound: {e}"));
            return;
        }
    };
    let _ = enc.set_bitrate(Bitrate::Bits(128_000));
    let ssrc = sender.audio_ssrc;
    let resume = cont.audio.lock().unwrap().take();
    let mut seq: u16 = resume.map(|r| r.0).unwrap_or_else(rand_u16);
    let mut ts: u32 = resume.map(|r| r.1).unwrap_or_else(|| (rand_u16() as u32) << 16);
    let mut out = vec![0u8; 4000];
    let (mut packets, mut octets) = (0u32, 0u32);
    let mut last_sr = Instant::now();
    let mut last_sent: Option<Instant> = resume.map(|r| r.2);
    let stop = Arc::new(AtomicBool::new(false));
    // sysaudio watches its own flag; mirror ours into it.
    let watcher = {
        let (stop, sh) = (stop.clone(), sh.clone());
        spawn("tinycord-pcsound-stop", move || {
            while !sh.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    let res = crate::sysaudio::run(stop, |chunk, _| {
        let mut marker = false;
        let gap = last_sent.map(|t| t.elapsed()).unwrap_or_default();
        if last_sent.is_some() && gap > Duration::from_millis(60) {
            // Silence in between: move the clock on so timing stays right.
            let extra = (gap - Duration::from_millis(20)).as_micros() as u64 * 48 / 1000;
            ts = ts.wrapping_add((extra / 960 * 960) as u32);
            marker = true;
        }
        let Ok(n) = enc.encode_float(chunk, &mut out) else { return };
        let Some(frame) = sender.e2ee(davey::MediaType::AUDIO, davey::Codec::OPUS, &out[..n]) else { return };
        let header = rtp::rtp_header(rtp::OPUS_PT, marker, seq, ts, ssrc, false);
        let _ = sender.send(&header, &frame);
        packets = packets.wrapping_add(1);
        octets = octets.wrapping_add(frame.len() as u32);
        last_sent = Some(Instant::now());
        if last_sr.elapsed() >= Duration::from_secs(1) {
            last_sr = Instant::now();
            let (h, b) = rtp::sender_report(ssrc, rtp::ntp_now(), ts, packets, octets);
            let _ = sender.send(&h, &b);
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(960);
    });
    if let Some(t) = last_sent {
        *cont.audio.lock().unwrap() = Some((seq, ts, t));
    }
    if let Err(msg) = res {
        tracing::warn!("{msg}");
        live_mut(|l| l.sound_note = msg);
    }
    let _ = watcher.join();
}

fn rand_u16() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(Instant::now().elapsed().as_nanos() as u64);
    h.finish() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_caps() {
        assert_eq!(allowed(0, 0), (720, 30));
        assert_eq!(allowed(0, 1), (720, 60));
        assert_eq!(allowed(0, 2), (1080, 60));
        assert_eq!(allowed(3, 0), (1080, 60));
        assert_eq!(allowed(2, 0), (0, 60));
        assert_eq!(allowed(2, 3), (0, 60));
        let c = clamp(LiveCfg { monitor: 0, height: 1440, fps: 60, sound: true, quality: 1 }, allowed(0, 0));
        assert_eq!((c.height, c.fps), (720, 30));
        let c = clamp(LiveCfg { monitor: 0, height: 0, fps: 60, sound: true, quality: 9 }, allowed(0, 2));
        assert_eq!(c.quality, 3);
        assert!(target_bitrate(1920, 1080, 60, 1) > bitrate(1920, 1080, 60));
        assert_eq!((c.height, c.fps), (1080, 60));
        assert!(bitrate(1280, 720, 30) > 2_000_000 && bitrate(1920, 1080, 60) > bitrate(1920, 1080, 30));
    }

    #[test]
    fn slot_keeps_newest() {
        let s = Slot::new();
        assert!(s.put(1).is_none());
        assert_eq!(s.put(2), Some(1));
        assert_eq!(s.take(Duration::from_millis(1)), Some(2));
        assert_eq!(s.take(Duration::from_millis(1)), None);
    }

    /// The encoder settings the stream uses: speed, size of what comes out,
    /// and that every packet fits and decodes.
    #[test]
    fn encoder_speed_and_packets() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVSlices;
        use openh264::{OpenH264API, Timestamp};
        for &(w, h, fps) in &[(1280usize, 720usize, 30u32), (1920, 1080, 60)] {
            let mut cap = Capturer::new(0).unwrap();
            let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
            let mut scaled = Vec::new();
            let br = bitrate(w as u32, h as u32, fps);
            let mut enc = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(fps, br)).unwrap();
            let mut dec = openh264::decoder::Decoder::new().unwrap();
            let mut pk = VideoPacketizer::new(1234);
            let (mut bytes, mut frames, mut decoded, mut biggest) = (0usize, 0usize, 0usize, 0usize);
            let t = Instant::now();
            let n = 40;
            for i in 0..n {
                cap.next(0).unwrap();
                cap.with_frame(|src, sw, sh, pitch, _| {
                    convert::scale_bgra(src, sw, sh, pitch, w, h, &mut scaled);
                    convert::bgra_to_i420(&scaled, w, h, w * 4, &mut pic);
                });
                let (y, rest) = pic.data.split_at(w * h);
                let (u, v) = rest.split_at((w / 2) * (h / 2));
                let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
                let bs = enc.encode_at(&src, Timestamp::from_millis(i * 1000 / fps as u64)).unwrap();
                let au = bs.to_vec();
                if au.is_empty() {
                    continue;
                }
                frames += 1;
                bytes += au.len();
                for p in pk.packetize(&au, i as u32 * 3000) {
                    biggest = biggest.max(p.header.len() + p.body.len());
                }
                if let Ok(Some(_)) = dec.decode(&au) {
                    decoded += 1;
                }
            }
            let per = t.elapsed() / n as u32;
            println!(
                "{w}x{h}@{fps}: {per:?}/frame (incl. scale+convert), {frames} frames, avg {} KB, {:.1} Mbit/s, largest packet {biggest}, decoded {decoded}",
                bytes / frames.max(1) / 1024,
                bytes as f64 * 8.0 * fps as f64 / frames.max(1) as f64 / 1e6
            );
            assert!(biggest <= rtp::MTU + 40, "packet too big: {biggest}");
            assert_eq!(decoded, frames);
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Encode time alone, size-limited slices vs. one slice per picture.
    #[test]
    #[ignore]
    fn encode_only() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVSlices;
        use openh264::{OpenH264API, Timestamp};
        let (w, h, fps) = (1920usize, 1080usize, 60u32);
        // Pre-render a moving picture (scrolling gradient + noise-free text-like bars).
        let mut frames = Vec::new();
        for t in 0..30 {
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let o = (y * w + x) * 4;
                    let bar = ((x + t * 12) / 7 % 5 == 0) as u8 * 200;
                    bgra[o..o + 4].copy_from_slice(&[(x / 8) as u8, (y / 5) as u8 ^ bar, ((x + y + t * 6) / 9) as u8, 255]);
                }
            }
            let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
            convert::bgra_to_i420(&bgra, w, h, w * 4, &mut pic);
            frames.push(pic);
        }
        for sliced in [false, true] {
            let mut cfg = encoder_config(fps, bitrate(w as u32, h as u32, fps)).slice_count(4).num_threads(4);
            if !sliced {
                cfg = openh264::encoder::EncoderConfig::new()
                    .usage_type(openh264::encoder::UsageType::CameraVideoRealTime)
                    .rate_control_mode(openh264::encoder::RateControlMode::Bitrate)
                    .bitrate(openh264::encoder::BitRate::from_bps(bitrate(w as u32, h as u32, fps)))
                    .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
                    .skip_frames(true)
                    .complexity(openh264::encoder::Complexity::Low)
                    .num_threads(2);
            }
            let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();
            let t = Instant::now();
            let mut bytes = 0;
            for (i, pic) in frames.iter().enumerate() {
                let (y, rest) = pic.data.split_at(w * h);
                let (u, v) = rest.split_at((w / 2) * (h / 2));
                let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
                bytes += enc.encode_at(&src, Timestamp::from_millis(i as u64 * 16)).unwrap().to_vec().len();
            }
            println!("sliced={sliced}: {:?}/frame, {} KB total", t.elapsed() / frames.len() as u32, bytes / 1024);
            // Same picture again and again (a still desktop).
            let pic = &frames[frames.len() - 1];
            let (y, rest) = pic.data.split_at(w * h);
            let (u, v) = rest.split_at((w / 2) * (h / 2));
            let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
            let t = Instant::now();
            let mut rb = 0;
            for i in 0..30u64 {
                rb += enc.encode_at(&src, Timestamp::from_millis(1000 + i * 16)).unwrap().to_vec().len();
            }
            println!("  repeat of an unchanged picture: {:?}/frame, {} bytes each", t.elapsed() / 30, rb / 30);
        }
    }
}

#[cfg(test)]
mod still_screen {
    use super::*;

    /// What comes out when the same picture is encoded again and again
    /// (a desktop app where nothing moves).
    #[test]
    fn repeats_produce_frames() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVSlices;
        use openh264::{OpenH264API, Timestamp};
        let (w, h, fps) = (1280usize, 720usize, 30u32);
        let mut cap = Capturer::new(0).unwrap();
        let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
        cap.with_frame(|src, sw, sh, pitch, _| convert::bgra_to_i420(src, sw, sh, pitch, &mut pic));
        assert_eq!((pic.w, pic.h), (w, h));
        let mut enc = Encoder::with_api_config(OpenH264API::from_source(), encoder_config(fps, bitrate(w as u32, h as u32, fps))).unwrap();
        let mut out = Vec::new();
        for i in 0..20u64 {
            if i == 12 {
                enc.force_intra_frame();
            }
            let (y, rest) = pic.data.split_at(w * h);
            let (u, v) = rest.split_at((w / 2) * (h / 2));
            let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
            // Repeats come every 250 ms.
            let bs = enc.encode_at(&src, Timestamp::from_millis(i * 250)).unwrap();
            out.push((bs.to_vec().len(), format!("{:?}", bs.frame_type())));
        }
        println!("{out:?}");
        // Every repeat must produce a frame (an empty one would send nothing).
        assert!(out.iter().all(|(n, kind)| *n > 0 && kind != "Skip"), "{out:?}");
    }
}

#[cfg(test)]
mod e2ee_budget {
    use super::*;

    /// The end-to-end encryption describes each NAL unit of a frame in a
    /// trailer that must stay under 255 bytes (about 4 bytes per NAL), or the
    /// frame can't be sent. Keep NAL counts tiny even for big keyframes.
    #[test]
    fn few_nal_units_per_frame() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVSlices;
        use openh264::{OpenH264API, Timestamp};
        // 4 slices = what a PC with 8+ threads uses.
        for &(w, h) in &[(1920usize, 1080usize), (2560, 1440)] {
            let cfg = encoder_config(60, bitrate(w as u32, h as u32, 60)).slice_count(4).num_threads(4);
            let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();
            let mut worst = 0;
            let mut biggest = 0;
            for t in 0..12usize {
                // Busy picture: fine text-like detail that changes every frame.
                let mut bgra = vec![0u8; w * h * 4];
                for y in 0..h {
                    for x in 0..w {
                        let o = (y * w + x) * 4;
                        let v = ((x * 7 + y * 13 + t * 31) ^ (x * y / 3)) as u8;
                        bgra[o..o + 4].copy_from_slice(&[v, v.rotate_left(3), v ^ 0x5a, 255]);
                    }
                }
                let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
                convert::bgra_to_i420(&bgra, w, h, w * 4, &mut pic);
                if t == 6 {
                    enc.force_intra_frame();
                }
                let (y, rest) = pic.data.split_at(w * h);
                let (u, v) = rest.split_at((w / 2) * (h / 2));
                let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
                let au = enc.encode_at(&src, Timestamp::from_millis(t as u64 * 16)).unwrap().to_vec();
                let nals = rtp::split_nalus(&au).len();
                worst = worst.max(nals);
                biggest = biggest.max(au.len());
            }
            println!("{w}x{h}: at most {worst} NAL units per frame (largest frame {} KB)", biggest / 1024);
            assert!(worst <= 8, "{w}x{h}: {worst} NAL units in one frame");
        }
    }
}

#[cfg(test)]
mod clock {
    use super::*;

    /// The capture clock hands the encoder exactly `fps` jobs a second, with
    /// evenly spaced timestamps, whether or not the screen changes.
    #[test]
    fn steady_ticks() {
        let fps = 30;
        let cfg = LiveCfg { monitor: 0, height: 480, fps, sound: false, quality: 1 };
        let sh = Arc::new(Shared {
            stop: AtomicBool::new(false),
            video_ssrc: 1,
            keyframe: AtomicBool::new(false),
            nacks: Mutex::new(Vec::new()),
            bitrate: AtomicU32::new(0),
            fps,
            quality: 1,
            size: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            loss: Mutex::new(Vec::new()),
            remb: AtomicU64::new(0),
            st: Stats::default(),
        });
        let slot = Arc::new(Slot::<Job>::new());
        let (_free_tx, free_rx) = mpsc::channel::<I420>();
        let t = {
            let (sh, slot) = (sh.clone(), slot.clone());
            std::thread::spawn(move || capture_thread(cfg, sh, slot, free_rx))
        };
        let start = Instant::now();
        let mut stamps = Vec::new();
        while start.elapsed() < Duration::from_secs(2) {
            match slot.take(Duration::from_millis(100)) {
                Some(Job::New(f)) => stamps.push(f.at),
                Some(Job::Repeat(at)) => stamps.push(at),
                None => {}
            }
        }
        sh.stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        let gaps: Vec<f64> = stamps.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0).collect();
        let n = stamps.len();
        println!("{n} jobs in 2 s; gaps min {:.2} ms max {:.2} ms", gaps.iter().cloned().fold(f64::MAX, f64::min), gaps.iter().cloned().fold(0.0, f64::max));
        assert!((55..=61).contains(&n), "{n} jobs");
        assert!(gaps.iter().all(|g| (g - 1000.0 / fps as f64).abs() < 0.5), "uneven: {gaps:?}");
    }
}

#[cfg(test)]
mod quality_bench {
    use super::*;

    /// Picture quality (PSNR, higher = closer to the original) and speed for
    /// encoder effort levels and bitrates, on a moving detailed picture.
    #[test]
    #[ignore]
    fn effort_vs_quality() {
        use openh264::encoder::{Complexity, Encoder, UsageType};
        use openh264::formats::{YUVSlices, YUVSource};
        use openh264::{OpenH264API, Timestamp};
        let (w, h, fps) = (1920usize, 1080usize, 60u32);
        let mut frames = Vec::new();
        for t in 0..40usize {
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let o = (y * w + x) * 4;
                    // scrolling "text" rows over a moving gradient
                    let sx = x + t * 9;
                    let text = (y % 18 < 12) && ((sx / 3) * 7919 + (y / 18) * 104729) % 11 < 4;
                    let base = [((sx + y) / 6) as u8, (y / 4) as u8, ((sx * 2) / 9) as u8];
                    let px = if text { [20, 20, 20] } else { base };
                    bgra[o..o + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
            convert::bgra_to_i420(&bgra, w, h, w * 4, &mut pic);
            frames.push(pic);
        }
        for (name, cx, usage, mult) in [
            ("low     6.9M", Complexity::Low, UsageType::CameraVideoRealTime, 1.0),
            ("medium  6.9M", Complexity::Medium, UsageType::CameraVideoRealTime, 1.0),
            ("high    6.9M", Complexity::High, UsageType::CameraVideoRealTime, 1.0),
            ("medium  11M ", Complexity::Medium, UsageType::CameraVideoRealTime, 1.6),
            ("medium  15M ", Complexity::Medium, UsageType::CameraVideoRealTime, 2.2),
            ("screen  6.9M", Complexity::Medium, UsageType::ScreenContentRealTime, 1.0),
        ] {
            let br = (bitrate(w as u32, h as u32, fps) as f64 * mult) as u32;
            let cfg = encoder_config(fps, br).slice_count(4).num_threads(4).complexity(cx).usage_type(usage);
            let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).unwrap();
            let mut dec = openh264::decoder::Decoder::new().unwrap();
            let (mut psnr_sum, mut n, mut bytes) = (0.0, 0, 0);
            let t = Instant::now();
            for (i, pic) in frames.iter().enumerate() {
                let (y, rest) = pic.data.split_at(w * h);
                let (u, v) = rest.split_at((w / 2) * (h / 2));
                let src = YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2));
                let au = enc.encode_at(&src, Timestamp::from_millis(i as u64 * 16)).unwrap().to_vec();
                bytes += au.len();
                if let Ok(Some(img)) = dec.decode(&au) {
                    if i >= 10 {
                        let (sy, ..) = img.strides();
                        let dy = img.y();
                        let mut se = 0f64;
                        for row in 0..h {
                            for col in 0..w {
                                let d = dy[row * sy + col] as f64 - y[row * w + col] as f64;
                                se += d * d;
                            }
                        }
                        let mse = se / (w * h) as f64;
                        psnr_sum += 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
                        n += 1;
                    }
                }
            }
            let per = t.elapsed() / frames.len() as u32;
            println!(
                "{name}: {per:?}/frame (encode+decode), PSNR {:.2} dB, actual {:.1} Mbit/s",
                psnr_sum / n.max(1) as f64,
                bytes as f64 * 8.0 * fps as f64 / frames.len() as f64 / 1e6
            );
        }
    }
}
