//! The Go Live connection: watching someone's screen share opens a second
//! connection to a voice server, which sends that stream's video (handed to
//! `video`) and audio (mixed into the speakers).

use crate::state::{self, state, Watching};
use crate::{video, voice};
use async_trait::async_trait;
use serde_json::Value;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::{Config, ConnectionInfo, Driver};
use std::num::{NonZeroU64, NonZeroU8};
use tokio::sync::Mutex;

struct Watch {
    key: String,
    user: u64,
    server_id: Option<u64>,
    server: Option<(String, String)>, // (endpoint, token)
    driver: Option<Driver>,
    connected_with: Option<(String, String)>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::const_new(None);

pub fn key_for(guild: u64, channel: u64, user: u64) -> String {
    format!("guild:{guild}:{channel}:{user}")
}

fn id(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()).unwrap_or(0)
}

fn set_watch_status(s: impl Into<String>) {
    let s = s.into();
    tracing::info!("stream: {s}");
    if let Some(w) = state().watching.as_mut() {
        w.status = s.clone();
    }
    video::info().lock().1 = s;
    state::changed();
}

/// Start watching (the caller sends the gateway "watch" message).
pub async fn begin(key: String, user: u64, name: String) {
    stop().await;
    tracing::info!("watching stream {key}");
    video::start(user);
    state().watching = Some(Watching { user, name, status: "Connecting to stream...".into() });
    state::changed();
    *WATCH.lock().await = Some(Watch {
        key,
        user,
        server_id: None,
        server: None,
        driver: None,
        connected_with: None,
    });
}

pub async fn current_key() -> Option<String> {
    WATCH.lock().await.as_ref().map(|w| w.key.clone())
}

/// Stop watching and free the decoder, connection and buffers.
pub async fn stop() {
    let old = WATCH.lock().await.take();
    if let Some(mut w) = old {
        if let Some(mut d) = w.driver.take() {
            d.leave();
        }
        tracing::info!("stopped watching {}", w.key);
    }
    video::stop();
    voice::shared().stream_out.lock().clear();
    if state().watching.take().is_some() {
        state::changed();
    }
}

pub async fn on_event(t: &str, d: &Value, session_id: Option<String>) {
    let key = d["stream_key"].as_str().unwrap_or("");
    let mut guard = WATCH.lock().await;
    let Some(w) = guard.as_mut() else { return };
    if key != w.key {
        return;
    }
    match t {
        "STREAM_CREATE" => {
            w.server_id = Some(id(&d["rtc_server_id"])).filter(|x| *x != 0);
            if d["paused"].as_bool() == Some(true) {
                drop(guard);
                set_watch_status("Stream is paused");
                return;
            }
        }
        "STREAM_SERVER_UPDATE" => {
            if let (Some(ep), Some(tok)) = (d["endpoint"].as_str(), d["token"].as_str()) {
                w.server = Some((ep.to_string(), tok.to_string()));
            }
        }
        "STREAM_UPDATE" => {
            let paused = d["paused"].as_bool() == Some(true);
            drop(guard);
            set_watch_status(if paused { "Stream is paused" } else { "" });
            return;
        }
        "STREAM_DELETE" => {
            let reason = d["reason"].as_str().unwrap_or("ended").to_string();
            drop(guard);
            stop().await;
            state::set_status(match reason.as_str() {
                "stream_full" => "That stream is full".to_string(),
                "unauthorized" => "Not allowed to watch that stream".to_string(),
                "user_requested" | "stream_ended" | "ended" => "Stream ended".to_string(),
                r => format!("Stream ended ({r})"),
            });
            return;
        }
        _ => return,
    }
    try_connect(w, session_id).await;
}

async fn try_connect(w: &mut Watch, session_id: Option<String>) {
    let (Some(server_id), Some((endpoint, token)), Some(sid)) =
        (w.server_id, w.server.clone(), session_id)
    else {
        return;
    };
    let key = (endpoint.clone(), token.clone());
    if w.connected_with.as_ref() == Some(&key) {
        return;
    }
    let me = state().me;
    // The stream server is addressed by rtc_server_id; DAVE uses that id
    // minus one as its channel (same as the official client).
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
    let driver = w.driver.get_or_insert_with(|| {
        let config = Config::default()
            .decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz48000)))
            .playout_buffer_length(NonZeroU8::new(3).unwrap())
            .playout_spike_length(2)
            .use_softclip(false)
            .video_receive(true);
        let mut d = Driver::new(config);
        for ev in [CoreEvent::VoiceTick, CoreEvent::DriverConnect, CoreEvent::DriverDisconnect] {
            d.add_global_event(Event::Core(ev), StreamRx);
        }
        d
    });
    set_watch_status("Connecting to stream...");
    match driver.connect(info).await {
        Ok(()) => {
            w.connected_with = Some(key);
            set_watch_status("Waiting for picture...");
        }
        Err(e) => set_watch_status(format!("Couldn't connect to the stream: {e}")),
    }
    let _ = w.user;
}

/// Stream audio and connection events.
struct StreamRx;

#[async_trait]
impl EventHandler for StreamRx {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick) => {
                let mut mix = [0i32; 960];
                let mut any = false;
                for data in tick.speaking.values() {
                    let Some(pcm) = &data.decoded_voice else { continue };
                    any = true;
                    for (m, s) in mix.iter_mut().zip(pcm.iter()) {
                        *m += *s as i32;
                    }
                }
                if any {
                    let out: Vec<i16> = mix.iter().map(|s| (*s).clamp(-32768, 32767) as i16).collect();
                    voice::shared().push_stream_out(&out);
                }
            }
            EventContext::DriverConnect(_) => tracing::info!("stream connection up"),
            EventContext::DriverDisconnect(d) => {
                tracing::warn!("stream connection dropped: {:?}", d.reason);
                set_watch_status("Stream connection dropped");
            }
            _ => {}
        }
        None
    }
}
