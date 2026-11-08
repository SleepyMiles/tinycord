#![doc(
    html_logo_url = "https://raw.githubusercontent.com/serenity-rs/songbird/current/songbird.png",
    html_favicon_url = "https://raw.githubusercontent.com/serenity-rs/songbird/current/songbird-ico.png"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
//! ![project logo][logo]
//!
//! Songbird is an async, cross-library compatible voice system for Discord, written in Rust.
//! The library offers:
//!  * A standalone gateway frontend compatible with [serenity] and [twilight] using the
//!    `"gateway"` and `"[serenity/twilight]"` plus `"[rustls/native]"` features. You can even run
//!    driverless, to help manage your [lavalink] sessions.
//!  * A standalone driver for voice calls, via the `"driver"` feature. If you can create
//!    a `ConnectionInfo` using any other gateway, or language for your bot, then you
//!    can run the songbird voice driver.
//!  * Voice receive and RT(C)P packet handling via the `"receive"` feature.
//!  * And, by default, a fully featured voice system featuring events, queues,
//!    seeking on compatible streams, shared multithreaded audio stream caches,
//!    and direct Opus data passthrough from DCA files.
//!
//! ## Intents
//! Songbird's gateway functionality requires you to specify the `GUILD_VOICE_STATES` intent.
//!
//! ## Examples
//! Full examples showing various types of functionality and integrations can be found
//! in [this crate's examples directory].
//!
//! ## Codec support
//! Songbird supports all [codecs and formats provided by Symphonia] (pure-Rust), with Opus support
//! provided by [opus2] (an FFI wrapper for libopus).
//!
//! **By default, *Songbird will not request any codecs from Symphonia*.** To change this, in your own
//! project you will need to depend on Symphonia as well.
//!
//! ```toml
//! # Including songbird alone gives you support for Opus via the DCA file format.
//! [dependencies.songbird]
//! version = "0.5"
//! features = ["builtin-queue"]
//!
//! # To get additional codecs, you *must* add Symphonia yourself.
//! # This includes the default formats (MKV/WebM, Ogg, Wave) and codecs (FLAC, PCM, Vorbis)...
//! [dependencies.symphonia]
//! version = "0.5"
//! features = ["aac", "mp3", "isomp4", "alac"] # ...as well as any extras you need!
//! ```
//!
//! ## Attribution
//!
//! Songbird's logo is based upon the copyright-free image ["Black-Capped Chickadee"] by George Gorgas White.
//!
//! [logo]: https://raw.githubusercontent.com/serenity-rs/songbird/current/songbird.png
//! [serenity]: https://github.com/serenity-rs/serenity
//! [twilight]: https://github.com/twilight-rs/twilight
//! [this crate's examples directory]: https://github.com/serenity-rs/songbird/tree/current/examples
//! ["Black-Capped Chickadee"]: https://www.oldbookillustrations.com/illustrations/black-capped-chickadee/
//! [`ConnectionInfo`]: struct@ConnectionInfo
//! [lavalink]: https://github.com/freyacodes/Lavalink
//! [codecs and formats provided by Symphonia]: https://github.com/pdeljanov/Symphonia#formats-demuxers
//! [opus2]: https://github.com/cijiugechu/opus2

#![warn(clippy::pedantic, rust_2018_idioms)]
#![allow(
    // Allowed as they are too pedantic
    clippy::module_name_repetitions,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    // TODO: would require significant rewriting of all existing docs
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_link_with_quotes,
    clippy::doc_markdown,
)]

mod config;
pub mod constants;

/// tinycord patch: extra user IDs the client already knows are in the voice
/// channel (from the main gateway). Discord may not send voice op 11
/// (ClientsConnect) to user accounts, which made DAVE reject proposals for
/// other members (songbird issue #310). These IDs are merged with the ones the
/// voice gateway reports.
pub static EXTRA_RECOGNIZED_USERS: std::sync::RwLock<Vec<u64>> = std::sync::RwLock::new(Vec::new());
#[cfg(feature = "driver")]
pub mod driver;

/// tinycord patch: receiving Go Live video on a connection made with
/// `Config::video_receive(true)`.
#[allow(missing_docs)]
pub mod video_hook {
    pub use davey;
    use std::sync::atomic::AtomicU32;
    use std::sync::RwLock;

    /// One transport-decrypted video RTP packet (still DAVE-encrypted).
    pub struct VideoRtp<'a> {
        pub ssrc: u32,
        pub seq: u16,
        pub timestamp: u32,
        pub marker: bool,
        pub payload_type: u8,
        /// RTP payload with header extensions and padding removed.
        pub body: &'a [u8],
    }

    pub type VideoSink = fn(&VideoRtp<'_>, &RwLock<Option<davey::DaveSession>>);

    /// Called for every non-Opus RTP packet on a video connection.
    pub static VIDEO_SINK: RwLock<Option<VideoSink>> = RwLock::new(None);
    /// Called with every text message on a watching connection's voice websocket.
    pub static WS_TAP: RwLock<Option<fn(&str)>> = RwLock::new(None);
    /// Called with every text message on our own stream's voice websocket.
    pub static WS_TAP_SEND: RwLock<Option<fn(&str)>> = RwLock::new(None);
    /// (video, rtx) SSRCs from our own stream connection's READY.
    static SEND_SSRCS: std::sync::Mutex<(u32, u32)> = std::sync::Mutex::new((0, 0));

    pub(crate) fn take_send_ssrcs() -> (u32, u32) {
        std::mem::take(&mut *SEND_SSRCS.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn tap(kind: u8, text: &str) {
        let hook = if kind == 2 { &WS_TAP_SEND } else { &WS_TAP };
        if kind == 2 && (text.contains("\"op\":2,") || text.contains("\"op\": 2,")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                if v["op"].as_u64() == Some(2) {
                    let s = &v["d"]["streams"][0];
                    let video = s["ssrc"].as_u64().unwrap_or(0) as u32;
                    let rtx = s["rtx_ssrc"].as_u64().unwrap_or(0) as u32;
                    *SEND_SSRCS.lock().unwrap_or_else(|e| e.into_inner()) = (video, rtx);
                }
            }
        }
        if let Some(f) = hook.read().ok().and_then(|g| *g) {
            f(text);
        }
    }
    /// Set to a media SSRC to send one RTCP PLI (keyframe request); 0 = none.
    pub static PLI_REQUEST: AtomicU32 = AtomicU32::new(0);
    /// Our SSRC on the video connection (RTCP sender SSRC).
    pub static SELF_SSRC: AtomicU32 = AtomicU32::new(0);

    /// Called with each decrypted RTCP packet on a `video_send` connection.
    pub static RTCP_SINK: RwLock<Option<fn(&[u8])>> = RwLock::new(None);

    /// Published when a `video_send` connection is up (replaced on reconnect).
    pub static SENDER: RwLock<Option<Arc<MediaSender>>> = RwLock::new(None);

    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;

    /// Sends our own RTP/RTCP on a stream connection.
    pub struct MediaSender {
        pub(crate) udp: std::net::UdpSocket,
        pub(crate) cipher: crate::driver::crypto_cipher::Cipher,
        counter: AtomicU32,
        pub(crate) dave: Arc<RwLock<Option<davey::DaveSession>>>,
        pub(crate) dave_version: Arc<AtomicU16>,
        pub(crate) ws: flume::Sender<crate::driver::tasks_message::WsMessage>,
        /// Our audio SSRC (from READY).
        pub audio_ssrc: u32,
        /// Video and retransmission SSRCs the server gave us (0 = not given).
        pub video_ssrc: u32,
        pub rtx_ssrc: u32,
    }

    impl MediaSender {
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn new(
            udp: std::net::UdpSocket,
            cipher: crate::driver::crypto_cipher::Cipher,
            dave: Arc<RwLock<Option<davey::DaveSession>>>,
            dave_version: Arc<AtomicU16>,
            ws: flume::Sender<crate::driver::tasks_message::WsMessage>,
            audio_ssrc: u32,
            (video_ssrc, rtx_ssrc): (u32, u32),
        ) -> Self {
            Self {
                udp,
                cipher,
                counter: AtomicU32::new(rand::random()),
                dave,
                dave_version,
                ws,
                audio_ssrc,
                video_ssrc,
                rtx_ssrc,
            }
        }

        /// True once the end-to-end encryption handshake allows sending
        /// (or the call doesn't use it).
        pub fn e2ee_ready(&self) -> bool {
            if self.dave_version.load(Ordering::Relaxed) == 0 {
                return true;
            }
            let guard = self.dave.read().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().is_some_and(|s| s.is_ready())
        }

        /// Encrypt (transport layer) and send. `header` is the clear part
        /// (RTP header incl. the 4-byte extension header, or the first 8
        /// bytes of RTCP); `body` is everything after it.
        pub fn send(&self, header: &[u8], body: &[u8]) -> std::io::Result<usize> {
            let mut buf = Vec::with_capacity(header.len() + body.len() + 24);
            buf.extend_from_slice(header);
            buf.extend_from_slice(body);
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            self.cipher
                .encrypt_raw(&mut buf, header.len(), n)
                .map_err(|_| std::io::Error::other("encrypt"))?;
            self.udp.send(&buf)
        }

        /// Send a JSON message on the stream's voice websocket.
        pub fn send_ws(&self, text: String) -> bool {
            self.ws.send(crate::driver::tasks_message::WsMessage::Raw(text)).is_ok()
        }

        /// DAVE-encrypt a media frame. `Some(frame)` = send this; `None` =
        /// DAVE is on but not ready yet (drop the frame).
        pub fn e2ee(&self, media: davey::MediaType, codec: davey::Codec, frame: &[u8]) -> Option<Vec<u8>> {
            if self.dave_version.load(Ordering::Relaxed) == 0 {
                return Some(frame.to_vec());
            }
            let mut guard = self.dave.write().unwrap_or_else(|e| e.into_inner());
            let session = guard.as_mut()?;
            if !session.is_ready() {
                return None;
            }
            session.encrypt(media, codec, frame).ok().map(|c| c.into_owned())
        }
    }
}
pub mod error;
#[cfg(feature = "driver")]
pub mod events;
mod float_duration;
#[cfg(feature = "gateway")]
mod handler;
pub mod id;
pub(crate) mod info;
#[cfg(feature = "driver")]
pub mod input;
#[cfg(feature = "gateway")]
pub mod join;
#[cfg(feature = "gateway")]
mod manager;
#[cfg(feature = "serenity")]
pub mod serenity;
#[cfg(feature = "gateway")]
pub mod shards;
#[cfg(any(test, feature = "internals"))]
pub mod test_utils;
#[cfg(feature = "driver")]
pub mod tracks;
#[cfg(feature = "driver")]
mod ws;

#[cfg(all(feature = "driver", feature = "receive"))]
pub use discortp as packet;
#[cfg(feature = "driver")]
pub use serenity_voice_model as model;

pub(crate) use serde_json as json;

#[cfg(feature = "driver")]
pub use crate::{
    driver::Driver,
    events::{CoreEvent, Event, EventContext, EventHandler, TrackEvent},
};

#[cfg(feature = "gateway")]
pub use crate::{handler::*, manager::*};

#[cfg(feature = "serenity")]
pub use crate::serenity::*;

pub use config::Config;
pub use float_duration::FloatDuration;
pub use info::ConnectionInfo;
