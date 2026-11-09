//! Sending side of Go Live: H.264 frames -> RTP packets (single NAL / FU-A,
//! like Discord's raw-UDP clients), a short history for resending lost
//! packets (RTX), and RTCP: reading viewers' feedback, writing sender reports.

use std::collections::VecDeque;

/// Same values we offer in SELECT_PROTOCOL (see the songbird patch).
pub const H264_PT: u8 = 103;
pub const RTX_PT: u8 = 104;
pub const OPUS_PT: u8 = 120;

/// Media bytes per RTP packet (Discord's clients use 1200).
pub const MTU: usize = 1200;

/// One-byte header extension "playout-delay" (id 5) = 0, which Discord's
/// clients put on every video packet. Sent encrypted, after the header.
pub const EXT_HEADER: [u8; 4] = [0xBE, 0xDE, 0x00, 0x01];
pub const EXT_BODY: [u8; 4] = [0x51, 0x00, 0x00, 0x00];

/// An RTP packet before transport encryption: `header` is sent in the
/// clear (12 bytes + extension header), `body` gets encrypted.
#[derive(Clone)]
pub struct Packet {
    pub header: Vec<u8>,
    pub body: Vec<u8>,
}

pub fn rtp_header(pt: u8, marker: bool, seq: u16, ts: u32, ssrc: u32, ext: bool) -> Vec<u8> {
    let mut h = Vec::with_capacity(16);
    h.push(0x80 | if ext { 0x10 } else { 0 });
    h.push(pt | if marker { 0x80 } else { 0 });
    h.extend_from_slice(&seq.to_be_bytes());
    h.extend_from_slice(&ts.to_be_bytes());
    h.extend_from_slice(&ssrc.to_be_bytes());
    if ext {
        h.extend_from_slice(&EXT_HEADER);
    }
    h
}

/// Split an Annex B access unit into NAL units (without start codes).
pub fn split_nalus(au: &[u8]) -> Vec<&[u8]> {
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
            // Drop only the first byte of a following 4-byte start code. A
            // NAL can end in a zero byte when it's encrypted, so never strip
            // more than one (same rule as WebRTC's packetizer).
            if starts.get(k + 1).is_some() && e > s && au[e - 1] == 0 {
                e -= 1;
            }
            &au[s..e]
        })
        .filter(|n| !n.is_empty())
        .collect()
}

pub struct VideoPacketizer {
    pub ssrc: u32,
    seq: u16,
}

impl VideoPacketizer {
    pub fn new(ssrc: u32) -> Self {
        Self { ssrc, seq: rand_u16() }
    }

    /// Carry on numbering from an earlier packetizer (same stream).
    pub fn with_seq(ssrc: u32, seq: u16) -> Self {
        Self { ssrc, seq }
    }

    /// Sequence number the next packet will get.
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// `frame` is the (DAVE-encrypted) Annex B frame, `ts` its 90 kHz time.
    pub fn packetize(&mut self, frame: &[u8], ts: u32) -> Vec<Packet> {
        let nalus = split_nalus(frame);
        let mut out = Vec::new();
        for (k, nalu) in nalus.iter().enumerate() {
            let last_nal = k == nalus.len() - 1;
            if nalu.len() <= MTU {
                let mut body = EXT_BODY.to_vec();
                body.extend_from_slice(nalu);
                out.push(Packet { header: self.header(last_nal, ts), body });
            } else {
                let (hdr, data) = (nalu[0], &nalu[1..]);
                let chunks: Vec<&[u8]> = data.chunks(MTU - 2).collect();
                for (i, c) in chunks.iter().enumerate() {
                    let (first, last) = (i == 0, i == chunks.len() - 1);
                    let mut fu = hdr & 0x1f;
                    if first {
                        fu |= 0x80;
                    }
                    if last {
                        fu |= 0x40;
                    }
                    let mut body = EXT_BODY.to_vec();
                    body.push((hdr & 0xe0) | 28);
                    body.push(fu);
                    body.extend_from_slice(c);
                    out.push(Packet { header: self.header(last_nal && last, ts), body });
                }
            }
        }
        out
    }

    fn header(&mut self, marker: bool, ts: u32) -> Vec<u8> {
        let h = rtp_header(H264_PT, marker, self.seq, ts, self.ssrc, true);
        self.seq = self.seq.wrapping_add(1);
        h
    }
}

/// Recently sent video packets, for resending when a viewer reports loss.
pub struct History {
    items: VecDeque<(u16, Packet)>,
    cap: usize,
    rtx_seq: u16,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self { items: VecDeque::with_capacity(cap), cap, rtx_seq: rand_u16() }
    }

    pub fn push(&mut self, p: &Packet) {
        let seq = u16::from_be_bytes([p.header[2], p.header[3]]);
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back((seq, p.clone()));
    }

    /// The original packet `seq` (to resend as-is when there's no RTX).
    pub fn get(&self, seq: u16) -> Option<Packet> {
        self.items.iter().rev().find(|(s, _)| *s == seq).map(|(_, p)| p.clone())
    }

    /// An RTX packet (RFC 4588) carrying the original packet `seq`.
    pub fn rtx(&mut self, seq: u16, rtx_ssrc: u32) -> Option<Packet> {
        let (_, p) = self.items.iter().rev().find(|(s, _)| *s == seq)?;
        let marker = p.header[1] & 0x80 != 0;
        let ts = u32::from_be_bytes([p.header[4], p.header[5], p.header[6], p.header[7]]);
        let header = rtp_header(RTX_PT, marker, self.rtx_seq, ts, rtx_ssrc, true);
        self.rtx_seq = self.rtx_seq.wrapping_add(1);
        // Extension body stays first; then the original sequence number.
        let mut body = p.body[..4].to_vec();
        body.extend_from_slice(&seq.to_be_bytes());
        body.extend_from_slice(&p.body[4..]);
        Some(Packet { header, body })
    }
}

/// What a viewer (via Discord's server) asked of us.
#[derive(Debug, PartialEq)]
pub enum Feedback {
    /// PLI or FIR: send a keyframe.
    Keyframe,
    /// Generic NACK: these sequence numbers were lost.
    Nack(Vec<u16>),
    /// Receiver report: share of our packets lost since the last report
    /// (0-255 = 0-100%).
    Loss(u8),
    /// The server's estimate of how much it can take from us (bits/s).
    Remb(u64),
}

/// Parse a (compound) RTCP packet for feedback about `media_ssrc`.
pub fn parse_feedback(buf: &[u8], media_ssrc: u32) -> Vec<Feedback> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= buf.len() {
        let fmt = buf[i] & 0x1f;
        let pt = buf[i + 1];
        let len = (u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize + 1) * 4;
        if i + len > buf.len() {
            break;
        }
        let pkt = &buf[i..i + len];
        let media = if len >= 12 { u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]) } else { 0 };
        match (pt, fmt) {
            // SR / RR report blocks (24 bytes each) about our stream
            (200, rc) | (201, rc) => {
                let mut k = if pt == 200 { 28 } else { 8 };
                for _ in 0..rc {
                    if k + 24 > pkt.len() {
                        break;
                    }
                    if u32::from_be_bytes([pkt[k], pkt[k + 1], pkt[k + 2], pkt[k + 3]]) == media_ssrc {
                        out.push(Feedback::Loss(pkt[k + 4]));
                    }
                    k += 24;
                }
            }
            // PSFB application layer: REMB bandwidth estimate
            (206, 15) if pkt.len() >= 20 && &pkt[12..16] == b"REMB" => {
                let exp = (pkt[17] >> 2) as u32;
                let mantissa = (((pkt[17] & 3) as u64) << 16) | ((pkt[18] as u64) << 8) | pkt[19] as u64;
                out.push(Feedback::Remb(mantissa << exp.min(40)));
            }
            // PSFB PLI
            (206, 1) if media == media_ssrc => out.push(Feedback::Keyframe),
            // PSFB FIR: the target SSRC is in the FCI entries
            (206, 4) => {
                let mut k = 12;
                while k + 8 <= pkt.len() {
                    if u32::from_be_bytes([pkt[k], pkt[k + 1], pkt[k + 2], pkt[k + 3]]) == media_ssrc {
                        out.push(Feedback::Keyframe);
                    }
                    k += 8;
                }
            }
            // RTPFB generic NACK: (PID, BLP) pairs
            (205, 1) if media == media_ssrc => {
                let mut seqs = Vec::new();
                let mut k = 12;
                while k + 4 <= pkt.len() {
                    let pid = u16::from_be_bytes([pkt[k], pkt[k + 1]]);
                    let blp = u16::from_be_bytes([pkt[k + 2], pkt[k + 3]]);
                    seqs.push(pid);
                    for b in 0..16 {
                        if blp & (1 << b) != 0 {
                            seqs.push(pid.wrapping_add(b + 1));
                        }
                    }
                    k += 4;
                }
                out.push(Feedback::Nack(seqs));
            }
            _ => {}
        }
        i += len;
    }
    out
}

/// RTCP sender report: maps our RTP clock to wall-clock time, which is what
/// lets viewers keep the stream's audio and video in sync.
/// Returns (clear 8-byte header, body to encrypt).
pub fn sender_report(ssrc: u32, ntp: u64, rtp_ts: u32, packets: u32, bytes: u32) -> (Vec<u8>, Vec<u8>) {
    let mut h = vec![0x80, 200, 0x00, 0x06];
    h.extend_from_slice(&ssrc.to_be_bytes());
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&ntp.to_be_bytes());
    b.extend_from_slice(&rtp_ts.to_be_bytes());
    b.extend_from_slice(&packets.to_be_bytes());
    b.extend_from_slice(&bytes.to_be_bytes());
    (h, b)
}

/// Current wall-clock time as NTP 32.32 fixed point.
pub fn ntp_now() -> u64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() + 2_208_988_800; // 1900 -> 1970
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

fn rand_u16() -> u16 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (n.subsec_nanos() ^ (n.as_secs() as u32).rotate_left(7)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::Assembler;
    use openh264::formats::{YUVBuffer, YUVSource};
    use std::time::Instant;

    fn frame(w: usize, h: usize, t: usize) -> YUVBuffer {
        let mut yuv = vec![128u8; w * h * 3 / 2];
        for y in 0..h {
            for x in 0..w {
                let boxed = x > w / 3 && x < w / 2 && y > h / 3 && y < h / 2;
                yuv[y * w + x] = if boxed { 235 } else { 50 + (((x + y + t * 8) / 16) % 2) as u8 * 100 };
            }
        }
        YUVBuffer::from_vec(yuv, w, h)
    }

    /// Our sender -> our receiver: real OpenH264 frames go out as RTP, one
    /// packet is lost and recovered from the RTX history, and every frame
    /// decodes.
    #[test]
    fn send_receive_with_rtx() {
        let (w, h) = (960, 540);
        let mut enc = openh264::encoder::Encoder::new().unwrap();
        let mut pk = VideoPacketizer::new(0x1234_5678);
        let mut hist = History::new(2000);
        let mut asm = Assembler::new();
        let mut dec = openh264::decoder::Decoder::new().unwrap();
        let now = Instant::now();
        let (mut decoded, mut sent, mut resent, mut fu, mut single, mut produced) = (0, 0, 0, 0, 0, 0);
        for t in 0..30 {
            let au = enc.encode(&frame(w, h, t)).unwrap().to_vec();
            if au.is_empty() {
                continue; // the encoder skipped this frame
            }
            produced += 1;
            let ts = (t as u32) * 3000;
            let pkts = pk.packetize(&au, ts);
            for (k, p) in pkts.iter().enumerate() {
                hist.push(p);
                sent += 1;
                assert_eq!(p.header.len(), 16);
                assert!(p.body.len() <= MTU + 6);
                if p.body[4] & 0x1f == 28 {
                    fu += 1;
                } else {
                    single += 1;
                }
                let seq = u16::from_be_bytes([p.header[2], p.header[3]]);
                let marker = p.header[1] & 0x80 != 0;
                // Lose packet 1 of frame 5; resend it as RTX right away.
                let (seq, body) = if t == 5 && k == 1 {
                    let r = hist.rtx(seq, 0x9999).expect("in history");
                    resent += 1;
                    // Receiver side: RTX payload = ext + OSN + original.
                    let osn = u16::from_be_bytes([r.body[4], r.body[5]]);
                    assert_eq!(osn, seq);
                    (osn, r.body[6..].to_vec())
                } else {
                    (seq, p.body[4..].to_vec())
                };
                for f in asm.push(seq, marker, &body, now) {
                    if let Ok(Some(y)) = dec.decode(&f.data) {
                        assert_eq!(y.dimensions(), (w, h));
                        decoded += 1;
                    }
                }
            }
        }
        println!("sent {sent} packets ({single} single NAL, {fu} FU-A), resent {resent}, decoded {decoded} of {produced} frames, lost {}", asm.lost_frames);
        assert!(fu > 0 && single > 0);
        assert_eq!(asm.lost_frames, 0);
        assert_eq!(decoded, produced, "every frame sent should decode");
    }

    #[test]
    fn feedback_parsing() {
        let me: u32 = 0xAABB_CCDD;
        // PLI
        let mut pli = vec![0x81, 206, 0, 2, 0, 0, 0, 1];
        pli.extend_from_slice(&me.to_be_bytes());
        // NACK for 100, plus 101 and 103 via the bitmask
        let mut nack = vec![0x81, 205, 0, 3, 0, 0, 0, 1];
        nack.extend_from_slice(&me.to_be_bytes());
        nack.extend_from_slice(&100u16.to_be_bytes());
        nack.extend_from_slice(&0b101u16.to_be_bytes());
        // FIR for us
        let mut fir = vec![0x84, 206, 0, 4, 0, 0, 0, 1, 0, 0, 0, 0];
        fir.extend_from_slice(&me.to_be_bytes());
        fir.extend_from_slice(&[7, 0, 0, 0]);
        // PLI for someone else (ignored)
        let mut other = vec![0x81, 206, 0, 2, 0, 0, 0, 1];
        other.extend_from_slice(&1u32.to_be_bytes());
        // An empty RR (8 bytes), then an RR with one block about us (25% lost)
        let empty_rr = vec![0x80, 201, 0, 1, 0, 0, 0, 9];
        let mut rr = vec![0x81, 201, 0, 7, 0, 0, 0, 9];
        rr.extend_from_slice(&me.to_be_bytes());
        rr.extend_from_slice(&[64, 0, 0, 3]);
        rr.extend_from_slice(&[0u8; 16]);
        // REMB: 1 SSRC, exp 3, mantissa 250000 -> 2 Mbit/s
        let mut remb = vec![0x8f, 206, 0, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        remb.extend_from_slice(b"REMB");
        let m: u32 = 250_000;
        remb.extend_from_slice(&[1, (3 << 2) | (m >> 16) as u8, (m >> 8) as u8, m as u8]);
        remb.extend_from_slice(&me.to_be_bytes());
        let compound: Vec<u8> = [pli, nack, fir, other, empty_rr, rr, remb].concat();
        let fb = parse_feedback(&compound, me);
        assert_eq!(
            fb,
            vec![
                Feedback::Keyframe,
                Feedback::Nack(vec![100, 101, 103]),
                Feedback::Keyframe,
                Feedback::Loss(64),
                Feedback::Remb(2_000_000)
            ]
        );
    }

    #[test]
    fn nalu_split() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let n = split_nalus(&au);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }
}

#[cfg(test)]
mod continuity {
    use super::*;

    /// A new packetizer for the same stream carries on the numbering, so a
    /// settings change mid-stream doesn't look like a jump to viewers.
    #[test]
    fn numbering_carries_on() {
        let mut a = VideoPacketizer::new(7);
        let first = a.packetize(&[0, 0, 0, 1, 0x65, 1, 2, 3], 0);
        let last_seq = u16::from_be_bytes([first.last().unwrap().header[2], first.last().unwrap().header[3]]);
        let mut b = VideoPacketizer::with_seq(7, a.next_seq());
        let next = b.packetize(&[0, 0, 0, 1, 0x41, 9], 3000);
        let seq = u16::from_be_bytes([next[0].header[2], next[0].header[3]]);
        assert_eq!(seq, last_seq.wrapping_add(1));
    }
}
