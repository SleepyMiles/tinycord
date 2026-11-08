//! Watching a Go Live stream: rebuild H.264 frames from RTP packets, remove
//! DAVE end-to-end encryption, decode with OpenH264, and scale the picture to
//! the viewer window. Everything here exists only while a stream is open.

use parking_lot::Mutex;
use songbird::video_hook::{self, davey, VideoRtp};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// What we offered the server (see the songbird patch).
pub const H264_PT: u8 = 103;
pub const RTX_PT: u8 = 104;

/// Libwebrtc's receiver rebuilds every NAL unit with a 4-byte start code,
/// and DAVE's sender encrypts assuming exactly that, so we must match.
const START_CODE: [u8; 4] = [0, 0, 0, 1];
const DAVE_MAGIC: [u8; 2] = [0xFA, 0xFA];

// ---------------------------------------------------------------------------
// RTP -> frames (RFC 6184, non-interleaved mode, as libwebrtc sends it)
// ---------------------------------------------------------------------------

struct Pkt {
    marker: bool,
    body: Vec<u8>,
    arrived: Instant,
}

pub struct Frame {
    /// Annex B bitstream (may still carry DAVE encryption).
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Frames were lost just before this one.
    pub after_loss: bool,
}

/// Collects packets (which may arrive out of order) and hands out whole
/// frames in order. A frame is the packets from just after the previous
/// frame's marker bit up to and including the next marker bit.
pub struct Assembler {
    packets: BTreeMap<u64, Pkt>,
    highest: Option<u64>,
    next_start: Option<u64>,
    gap: bool,
    pub lost_frames: u64,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self { packets: BTreeMap::new(), highest: None, next_start: None, gap: false, lost_frames: 0 }
    }

    /// Turn a 16-bit sequence number into a continuous 64-bit one.
    fn extend(&mut self, seq: u16) -> u64 {
        let ext = match self.highest {
            None => (1u64 << 32) + seq as u64,
            Some(h) => {
                let diff = seq.wrapping_sub(h as u16) as i16 as i64;
                (h as i64 + diff) as u64
            }
        };
        if self.highest.is_none_or(|h| ext > h) {
            self.highest = Some(ext);
        }
        ext
    }

    pub fn push(&mut self, seq: u16, marker: bool, body: &[u8], now: Instant) -> Vec<Frame> {
        let ext = self.extend(seq);
        if self.next_start.is_some_and(|s| ext < s) {
            return Vec::new(); // late or duplicate
        }
        self.packets.entry(ext).or_insert_with(|| Pkt { marker, body: body.to_vec(), arrived: now });
        self.assemble(now)
    }

    fn skip_through_marker(&mut self, from: u64) -> bool {
        let found = self.packets.range(from..).find(|(_, p)| p.marker).map(|(k, _)| *k);
        if let Some(m) = found {
            self.packets = self.packets.split_off(&(m + 1));
            self.next_start = Some(m + 1);
            true
        } else {
            false
        }
    }

    fn assemble(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        loop {
            let Some(start) = self.next_start else {
                // Not lined up yet. A keyframe starts with SPS (or an access
                // unit delimiter): if the earliest packet is one, start there;
                // otherwise the first full frame starts after a marker.
                if let Some((&k, p)) = self.packets.iter().next() {
                    if starts_keyframe(&p.body) {
                        self.next_start = Some(k);
                        continue;
                    }
                }
                if self.skip_through_marker(0) {
                    continue;
                }
                break;
            };
            let mut k = start;
            let mut end = None;
            while let Some(p) = self.packets.get(&k) {
                if p.marker {
                    end = Some(k);
                    break;
                }
                k += 1;
            }
            if let Some(end) = end {
                let pkts: Vec<Pkt> = (start..=end).filter_map(|k| self.packets.remove(&k)).collect();
                self.next_start = Some(end + 1);
                let after_loss = std::mem::take(&mut self.gap);
                match build_frame(&pkts) {
                    Some((data, keyframe)) => out.push(Frame { data, keyframe, after_loss }),
                    None => {
                        self.gap = true;
                        self.lost_frames += 1;
                    }
                }
                continue;
            }
            // Incomplete. Wait a little for late packets, then give up on it.
            let oldest = self.packets.values().map(|p| p.arrived).min();
            let stale = oldest.is_some_and(|t| now.duration_since(t) > Duration::from_millis(120));
            if stale || self.packets.len() > 3000 {
                self.gap = true;
                self.lost_frames += 1;
                if !self.skip_through_marker(start) {
                    self.packets.clear();
                    self.next_start = None;
                    break;
                }
                continue;
            }
            break;
        }
        out
    }
}

/// Does this packet begin with SPS or an access unit delimiter?
fn starts_keyframe(b: &[u8]) -> bool {
    match b.first().map(|h| h & 0x1f) {
        Some(7) | Some(9) => true,
        Some(24) => b.get(3).is_some_and(|h| matches!(h & 0x1f, 7 | 9)),
        _ => false,
    }
}

/// Packets of one frame -> Annex B bytes, plus "contains an IDR slice".
fn build_frame(pkts: &[Pkt]) -> Option<(Vec<u8>, bool)> {
    let total: usize = pkts.iter().map(|p| p.body.len() + 8).sum();
    let mut out = Vec::with_capacity(total);
    let mut keyframe = false;
    let mut fu_open = false;
    for p in pkts {
        let b = &p.body;
        if b.is_empty() {
            continue;
        }
        match b[0] & 0x1f {
            t @ 1..=23 => {
                keyframe |= t == 5;
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(b);
            }
            24 => {
                // STAP-A: [hdr] ([size16][nalu])*
                let mut i = 1;
                while i + 2 <= b.len() {
                    let n = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
                    i += 2;
                    if n == 0 || i + n > b.len() {
                        return None;
                    }
                    keyframe |= b[i] & 0x1f == 5;
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(&b[i..i + n]);
                    i += n;
                }
            }
            28 => {
                // FU-A: [indicator][fu header][fragment]
                if b.len() < 2 {
                    return None;
                }
                let (start, end, t) = (b[1] & 0x80 != 0, b[1] & 0x40 != 0, b[1] & 0x1f);
                if start {
                    if fu_open {
                        return None;
                    }
                    keyframe |= t == 5;
                    out.extend_from_slice(&START_CODE);
                    out.push((b[0] & 0xe0) | t);
                    fu_open = true;
                } else if !fu_open {
                    return None;
                }
                out.extend_from_slice(&b[2..]);
                if end {
                    fu_open = false;
                }
            }
            _ => return None, // STAP-B / MTAP / FU-B: not used by WebRTC
        }
    }
    (!fu_open && !out.is_empty()).then_some((out, keyframe))
}

// ---------------------------------------------------------------------------
// YUV -> scaled BGRA
// ---------------------------------------------------------------------------

/// A decoded picture (I420), owned.
#[derive(Default)]
pub struct Picture {
    pub w: usize,
    pub h: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub ys: usize,
    pub uvs: usize,
}

/// The picture ready to paint: `w`x`h` pixels, 0x00RRGGBB.
#[derive(Default)]
pub struct Rendered {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    pub src_w: usize,
    pub src_h: usize,
}

/// Fit `sw`x`sh` inside `bw`x`bh`, keeping the aspect ratio.
pub fn fit(sw: usize, sh: usize, bw: usize, bh: usize) -> (usize, usize) {
    if sw == 0 || sh == 0 || bw == 0 || bh == 0 {
        return (0, 0);
    }
    if bw * sh <= bh * sw {
        (bw, (bw * sh / sw).max(1))
    } else {
        ((bh * sw / sh).max(1), bh)
    }
}

/// Average 2x2 blocks: used before shrinking a lot, so text stays readable.
fn halve(src: &[u8], w: usize, h: usize, stride: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; nw * nh];
    for y in 0..nh {
        let r0 = &src[(2 * y).min(h - 1) * stride..];
        let r1 = &src[(2 * y + 1).min(h - 1) * stride..];
        let o = &mut out[y * nw..(y + 1) * nw];
        for x in 0..nw {
            let (a, b) = (2 * x, (2 * x + 1).min(w - 1));
            o[x] = ((r0[a] as u32 + r0[b] as u32 + r1[a] as u32 + r1[b] as u32 + 2) >> 2) as u8;
        }
    }
    (out, nw, nh)
}

/// Scale and colour-convert (BT.601 limited range, like libyuv) in one pass:
/// bilinear on brightness, nearest on colour.
pub fn render(pic: &Picture, dw: usize, dh: usize, out: &mut Rendered) {
    out.w = dw;
    out.h = dh;
    out.src_w = pic.w;
    out.src_h = pic.h;
    out.px.resize(dw * dh, 0);
    if dw == 0 || dh == 0 || pic.w == 0 {
        return;
    }
    // Shrinking by more than 2x: pre-average the brightness plane.
    let mut mip: Option<(Vec<u8>, usize, usize)> = None;
    loop {
        let next = {
            let (src, w, h, st) = match &mip {
                Some((b, w, h)) => (&b[..], *w, *h, *w),
                None => (&pic.y[..], pic.w, pic.h, pic.ys),
            };
            (w >= dw * 2 && h >= dh * 2).then(|| halve(src, w, h, st))
        };
        match next {
            Some(n) => mip = Some(n),
            None => break,
        }
    }
    let (yp, yw, yh, ys) = match &mip {
        Some((b, w, h)) => (&b[..], *w, *h, *w),
        None => (&pic.y[..], pic.w, pic.h, pic.ys),
    };
    // Column lookup: 16.16 fixed point source positions.
    let fx = |x: usize, sw: usize| -> i64 {
        (((x as i64 * 2 + 1) * sw as i64 * 65536) / (dw as i64 * 2)) - 32768
    };
    let cols: Vec<(usize, usize, u32, usize)> = (0..dw)
        .map(|x| {
            let p = fx(x, yw).clamp(0, ((yw - 1) as i64) << 16);
            let x0 = (p >> 16) as usize;
            let x1 = (x0 + 1).min(yw - 1);
            let f = (p & 0xffff) as u32;
            let cx = ((x * pic.w) / dw / 2).min(pic.w.div_ceil(2) - 1);
            (x0, x1, f, cx)
        })
        .collect();
    for y in 0..dh {
        let p = ((((y as i64 * 2 + 1) * yh as i64 * 65536) / (dh as i64 * 2)) - 32768)
            .clamp(0, ((yh - 1) as i64) << 16);
        let y0 = (p >> 16) as usize;
        let y1 = (y0 + 1).min(yh - 1);
        let fy = (p & 0xffff) as u32;
        let r0 = &yp[y0 * ys..];
        let r1 = &yp[y1 * ys..];
        let cy = ((y * pic.h) / dh / 2).min(pic.h.div_ceil(2) - 1);
        let ur = &pic.u[cy * pic.uvs..];
        let vr = &pic.v[cy * pic.uvs..];
        let row = &mut out.px[y * dw..(y + 1) * dw];
        for (o, &(x0, x1, f, cx)) in row.iter_mut().zip(cols.iter()) {
            let top = r0[x0] as u32 * (65536 - f) + r0[x1] as u32 * f;
            let bot = r1[x0] as u32 * (65536 - f) + r1[x1] as u32 * f;
            let luma = ((top as u64 * (65536 - fy) as u64 + bot as u64 * fy as u64) >> 32) as i32;
            let c = (luma - 16) * 298;
            let d = ur[cx] as i32 - 128;
            let e = vr[cx] as i32 - 128;
            let r = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u32;
            let g = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u32;
            let b = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u32;
            *o = (r << 16) | (g << 8) | b;
        }
    }
}

// ---------------------------------------------------------------------------
// Live state (one stream at a time)
// ---------------------------------------------------------------------------

struct Live {
    /// Streamer's user id (DAVE needs it to pick the right key).
    user: u64,
    ssrc: u32,
    asm: Assembler,
    need_key: bool,
    waiting_since: Instant,
    last_pli: Option<Instant>,
    tx: SyncSender<Vec<u8>>,
    packets: u64,
    frames: u64,
    dave_fail: u64,
    last_log: Instant,
    bad_pt_logged: bool,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

/// Latest picture for the window (shared with the UI thread).
pub fn rendered() -> &'static Mutex<Rendered> {
    static R: OnceLock<Mutex<Rendered>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Rendered::default()))
}

/// Stream volume 0.0..=2.0 (f32 bits) and "keep the viewer on top".
pub static STREAM_VOLUME: AtomicU32 = AtomicU32::new(0x3F80_0000); // 1.0
pub static STREAM_PIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Viewer window client size, set by the UI.
pub static TARGET_W: AtomicU32 = AtomicU32::new(960);
pub static TARGET_H: AtomicU32 = AtomicU32::new(540);

/// Called (from the decoder thread) when a new picture is ready.
pub static ON_FRAME: RwLock<Option<fn()>> = RwLock::new(None);

/// Server-chosen codec and our status text for the viewer window.
pub fn info() -> &'static Mutex<(String, String)> {
    static I: OnceLock<Mutex<(String, String)>> = OnceLock::new();
    I.get_or_init(|| Mutex::new((String::new(), "Connecting to stream...".into())))
}

fn set_info(s: impl Into<String>) {
    info().lock().1 = s.into();
    notify();
}

fn notify() {
    if let Some(f) = ON_FRAME.read().ok().and_then(|g| *g) {
        f();
    }
}

/// Start receiving video for `user`'s stream.
pub fn start(user: u64) {
    let (tx, rx) = sync_channel::<Vec<u8>>(6);
    *info().lock() = (String::new(), "Connecting to stream...".into());
    *rendered().lock() = Rendered::default();
    *LIVE.lock() = Some(Live {
        user,
        ssrc: 0,
        asm: Assembler::new(),
        need_key: true,
        waiting_since: Instant::now(),
        last_pli: None,
        tx,
        packets: 0,
        frames: 0,
        dave_fail: 0,
        last_log: Instant::now(),
        bad_pt_logged: false,
    });
    std::thread::Builder::new()
        .name("video".into())
        .spawn(move || decoder_thread(rx))
        .expect("video thread");
    if let Ok(mut s) = video_hook::VIDEO_SINK.write() {
        *s = Some(on_video);
    }
    if let Ok(mut t) = video_hook::WS_TAP.write() {
        *t = Some(on_ws);
    }
}

/// Stop and free everything (the decoder thread exits on its own).
pub fn stop() {
    if let Ok(mut s) = video_hook::VIDEO_SINK.write() {
        *s = None;
    }
    *LIVE.lock() = None;
    let mut r = rendered().lock();
    *r = Rendered::default();
    r.px.shrink_to_fit();
}

fn request_keyframe(l: &mut Live, now: Instant) {
    if l.ssrc != 0 && l.last_pli.is_none_or(|t| now.duration_since(t) > Duration::from_millis(900)) {
        video_hook::PLI_REQUEST.store(l.ssrc, Ordering::Relaxed);
        l.last_pli = Some(now);
    }
}

/// Runs on the voice library's network task for every video packet.
fn on_video(pkt: &VideoRtp<'_>, dave: &RwLock<Option<davey::DaveSession>>) {
    let mut guard = LIVE.lock();
    let Some(l) = guard.as_mut() else { return };
    if pkt.payload_type == RTX_PT {
        return; // retransmissions: we never ask for them
    }
    if pkt.payload_type != H264_PT {
        if !l.bad_pt_logged {
            tracing::warn!("video packet with payload type {} (expected {H264_PT})", pkt.payload_type);
            l.bad_pt_logged = true;
        }
        return;
    }
    if l.ssrc == 0 {
        l.ssrc = pkt.ssrc;
        tracing::info!("video: first packet, ssrc {}", pkt.ssrc);
    } else if pkt.ssrc != l.ssrc {
        return;
    }
    let now = Instant::now();
    l.packets += 1;
    for f in l.asm.push(pkt.seq, pkt.marker, pkt.body, now) {
        if f.after_loss {
            l.need_key = true;
            l.waiting_since = now;
        }
        let data = if f.data.ends_with(&DAVE_MAGIC) {
            let mut d = dave.write().unwrap_or_else(|e| e.into_inner());
            match d.as_mut().map(|s| s.decrypt(l.user, davey::MediaType::VIDEO, &f.data)) {
                Some(Ok(plain)) => plain,
                Some(Err(e)) => {
                    l.dave_fail += 1;
                    if l.dave_fail % 100 == 1 {
                        tracing::warn!("video DAVE decrypt failed #{}: {e:?}", l.dave_fail);
                    }
                    continue;
                }
                None => continue,
            }
        } else {
            f.data
        };
        if l.need_key {
            // After a gap the next frames reference what we lost: wait for a
            // keyframe (asking for one), but don't freeze for more than 3 s.
            if !f.keyframe && now.duration_since(l.waiting_since) < Duration::from_secs(3) {
                request_keyframe(l, now);
                continue;
            }
            l.need_key = false;
        }
        l.frames += 1;
        match l.tx.try_send(data) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Decoder is behind: drop and restart from a keyframe.
                l.need_key = true;
                l.waiting_since = now;
                request_keyframe(l, now);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
    if l.need_key {
        request_keyframe(l, now);
    }
    if now.duration_since(l.last_log) > Duration::from_secs(10) {
        tracing::info!(
            "video: {} packets, {} frames, {} lost, {} DAVE failures",
            l.packets, l.frames, l.asm.lost_frames, l.dave_fail
        );
        l.last_log = now;
    }
}

fn decoder_thread(rx: Receiver<Vec<u8>>) {
    use openh264::formats::YUVSource;
    let mut dec = match openh264::decoder::Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            set_info(format!("Video decoder failed to start: {e}"));
            return;
        }
    };
    set_info("Waiting for picture...");
    let mut pic = Picture::default();
    let mut scratch = Rendered::default();
    let mut errors = 0u64;
    let mut first = true;
    while let Ok(frame) = rx.recv() {
        match dec.decode(&frame) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                let (ys, uvs, _) = yuv.strides();
                pic.w = w;
                pic.h = h;
                pic.ys = ys;
                pic.uvs = uvs;
                pic.y.clear();
                pic.y.extend_from_slice(&yuv.y()[..ys * h]);
                let ch = h.div_ceil(2);
                pic.u.clear();
                pic.u.extend_from_slice(&yuv.u()[..uvs * ch]);
                pic.v.clear();
                pic.v.extend_from_slice(&yuv.v()[..uvs * ch]);
                if first {
                    tracing::info!("video: decoding {w}x{h}");
                    first = false;
                    set_info("");
                }
                let (bw, bh) =
                    (TARGET_W.load(Ordering::Relaxed) as usize, TARGET_H.load(Ordering::Relaxed) as usize);
                let (dw, dh) = fit(w, h, bw, bh);
                render(&pic, dw, dh, &mut scratch);
                std::mem::swap(&mut *rendered().lock(), &mut scratch);
                notify();
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                if errors % 50 == 1 {
                    tracing::warn!("video decode error #{errors}: {e}");
                }
                if let Some(l) = LIVE.lock().as_mut() {
                    l.need_key = true;
                    l.waiting_since = Instant::now();
                }
            }
        }
    }
    tracing::info!("video decoder stopped");
}

/// Messages on the stream's voice connection: which codec the server picked
/// and which SSRC carries the picture.
fn on_ws(text: &str) {
    if !(text.contains("video_codec") || text.contains("\"op\":12") || text.contains("\"op\": 12")) {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return };
    let d = &v["d"];
    if let Some(codec) = d["video_codec"].as_str() {
        tracing::info!("stream video codec: {codec}");
        info().lock().0 = codec.to_string();
        if codec != "H264" {
            set_info(format!("This stream is {codec}. tinycord can only show H.264 so far."));
        }
    }
    if v["op"].as_u64() == Some(12) {
        tracing::info!("stream video info: {d}");
        let ssrc = d["streams"]
            .as_array()
            .and_then(|s| s.iter().find(|x| x["active"].as_bool() != Some(false)))
            .and_then(|x| x["ssrc"].as_u64())
            .or_else(|| d["video_ssrc"].as_u64())
            .unwrap_or(0) as u32;
        if ssrc != 0 {
            if let Some(l) = LIVE.lock().as_mut() {
                if l.ssrc != ssrc {
                    l.ssrc = ssrc;
                    l.asm = Assembler::new();
                    l.need_key = true;
                    l.waiting_since = Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::{YUVBuffer, YUVSource};

    /// Split an Annex B access unit into NAL units.
    fn nalus(au: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .map(|(k, &s)| {
                let mut e = starts.get(k + 1).map(|n| n - 3).unwrap_or(au.len());
                while e > s && au[e - 1] == 0 {
                    e -= 1; // trailing zero of a 4-byte start code
                }
                &au[s..e]
            })
            .collect()
    }

    /// Packetize like libwebrtc: small NALs aggregated in STAP-A, big ones
    /// split into FU-A, marker on the last packet of the frame.
    fn packetize(au: &[u8], mtu: usize) -> Vec<(Vec<u8>, bool)> {
        let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
        let units = nalus(au);
        let mut stap: Vec<&[u8]> = Vec::new();
        let flush = |stap: &mut Vec<&[u8]>, out: &mut Vec<(Vec<u8>, bool)>| {
            match stap.len() {
                0 => {}
                1 => out.push((stap[0].to_vec(), false)),
                _ => {
                    let nri = stap.iter().map(|n| n[0] & 0x60).max().unwrap();
                    let mut p = vec![nri | 24];
                    for n in stap.iter() {
                        p.extend_from_slice(&(n.len() as u16).to_be_bytes());
                        p.extend_from_slice(n);
                    }
                    out.push((p, false));
                }
            }
            stap.clear();
        };
        for n in units {
            if n.len() <= mtu {
                let stap_size: usize = stap.iter().map(|x| x.len() + 2).sum::<usize>() + 1;
                if stap_size + n.len() + 2 > mtu {
                    flush(&mut stap, &mut out);
                }
                stap.push(n);
            } else {
                flush(&mut stap, &mut out);
                let (hdr, data) = (n[0], &n[1..]);
                let chunks: Vec<&[u8]> = data.chunks(mtu - 2).collect();
                for (k, c) in chunks.iter().enumerate() {
                    let mut fu = (hdr & 0x1f) as u8;
                    if k == 0 {
                        fu |= 0x80;
                    }
                    if k == chunks.len() - 1 {
                        fu |= 0x40;
                    }
                    let mut p = vec![(hdr & 0xe0) | 28, fu];
                    p.extend_from_slice(c);
                    out.push((p, false));
                }
            }
        }
        flush(&mut stap, &mut out);
        if let Some(last) = out.last_mut() {
            last.1 = true;
        }
        out
    }

    pub fn test_frame_t(w: usize, h: usize, t: usize) -> YUVBuffer {
        let mut yuv = vec![0u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                // Moving diagonal bands plus a sharp-edged box ("text").
                let band = (((x + y + t * 8) / 16) % 2) as u8;
                let boxed = x > w / 3 && x < w / 2 && y > h / 3 && y < h / 2;
                yuv[y * w + x] = if boxed { 235 } else { 60 + band * 100 };
            }
        }
        for i in w * h..yuv.len() {
            yuv[i] = 128;
        }
        YUVBuffer::from_vec(yuv, w, h)
    }

    /// Real H.264 from OpenH264's encoder -> RTP packets (reordered, with one
    /// lost packet) -> Assembler -> decoder -> scaled picture.
    #[test]
    fn rtp_h264_roundtrip() {
        let (w, h) = (640, 360);
        let mut enc = openh264::encoder::Encoder::new().unwrap();
        let mut asm = Assembler::new();
        let mut dec = openh264::decoder::Decoder::new().unwrap();
        let mut seq: u16 = 65_500; // also tests sequence wrap-around
        let now = Instant::now();
        let mut decoded = 0;
        let mut keyframes = 0;
        let mut frames_out = 0;
        let mut last_luma_box = 0u8;
        let mut lost_once = false;
        let mut packets_total = 0;
        // Same policy as on_video: after a loss, skip until a keyframe.
        let mut need_key = false;
        let mut decoded_after_recovery = 0;
        for t in 0..40 {
            if t == 26 {
                enc.force_intra_frame(); // what a PLI makes the streamer do
            }
            let src = test_frame_t(w, h, t);
            let au = enc.encode(&src).unwrap().to_vec();
            let pkts = packetize(&au, 1100);
            let n = pkts.len();
            let mut sent: Vec<(u16, Vec<u8>, bool)> =
                pkts.into_iter().enumerate().map(|(k, (b, m))| (seq.wrapping_add(k as u16), b, m)).collect();
            // Deliver two packets out of order.
            if sent.len() > 3 {
                sent.swap(1, 2);
            }
            for (k, (s, body, marker)) in sent.iter().enumerate() {
                let (s, marker) = (*s, *marker);
                packets_total += 1;
                if t == 20 && k == 0 && !lost_once {
                    lost_once = true; // drop the first packet of frame 20
                    continue;
                }
                for f in asm.push(s, marker, body, now + Duration::from_millis(t as u64 * 33)) {
                    frames_out += 1;
                    keyframes += f.keyframe as u32;
                    if f.after_loss {
                        need_key = true;
                    }
                    if need_key && !f.keyframe {
                        continue;
                    }
                    let recovered = need_key;
                    need_key = false;
                    if let Ok(Some(yuv)) = dec.decode(&f.data) {
                        decoded += 1;
                        if recovered || decoded_after_recovery > 0 {
                            decoded_after_recovery += 1;
                        }
                        let (dw, dh) = yuv.dimensions();
                        assert_eq!((dw, dh), (w, h));
                        let ys = yuv.strides().0;
                        last_luma_box = yuv.y()[(h * 5 / 12) * ys + w * 5 / 12];
                    }
                }
            }
            seq = seq.wrapping_add(n as u16);
        }
        // Frame 20 is lost; it is given up on once it's 120 ms stale.
        let late = now + Duration::from_secs(5);
        for f in asm.push(seq, true, &[0x09, 0x10], late) {
            frames_out += 1;
            let _ = dec.decode(&f.data);
        }
        println!(
            "rtp: {packets_total} packets, {frames_out} frames out, {keyframes} keyframes, {decoded} decoded, lost {}, box luma {last_luma_box}",
            asm.lost_frames
        );
        println!("rtp: decoded after recovering from the loss: {decoded_after_recovery}");
        assert!(keyframes >= 2, "the starting keyframe should be kept");
        assert!(decoded >= 30, "all frames except the lost stretch should decode");
        assert!(decoded_after_recovery >= 10, "should recover at the next keyframe");
        assert!(asm.lost_frames >= 1, "the damaged frame should be reported lost");
        assert!(last_luma_box > 200, "picture content survived");
    }

    #[test]
    fn scaling_and_color() {
        // White box on black, 1920x1080 -> 480x270 (with pre-averaging).
        let (w, h) = (1920, 1080);
        let mut pic = Picture { w, h, ys: w, uvs: w / 2, ..Default::default() };
        pic.y = vec![16; w * h];
        for y in 400..600 {
            for x in 800..1100 {
                pic.y[y * w + x] = 235;
            }
        }
        pic.u = vec![128; (w / 2) * (h / 2)];
        pic.v = vec![128; (w / 2) * (h / 2)];
        let mut out = Rendered::default();
        let (dw, dh) = fit(w, h, 480, 400);
        assert_eq!((dw, dh), (480, 270));
        let t = Instant::now();
        for _ in 0..10 {
            render(&pic, dw, dh, &mut out);
        }
        let per = t.elapsed() / 10;
        let mid = out.px[125 * dw + 237];
        let corner = out.px[5 * dw + 5];
        println!("render 1080p -> {dw}x{dh}: {per:?} per frame; box {mid:06x}, background {corner:06x}");
        assert_eq!(corner, 0x000000);
        assert!(mid & 0xff > 240 && (mid >> 16) & 0xff > 240);

        // And a 1080p -> 960x540 frame, the default window size.
        let t = Instant::now();
        render(&pic, 960, 540, &mut out);
        println!("render 1080p -> 960x540: {:?}", t.elapsed());
    }
}

