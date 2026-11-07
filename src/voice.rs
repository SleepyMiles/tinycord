//! Voice: the songbird driver (does the voice gateway, UDP, Opus and DAVE
//! encryption), wired to our mic/speaker buffers and the speaking indicator.

use crate::audio::{self, AudioHandle, MicSource, Shared};
use crate::state::{self, state, MicMode};
use async_trait::async_trait;
use parking_lot::Mutex;
use songbird::driver::{Bitrate, Channels, DecodeConfig, DecodeMode, MixMode, SampleRate};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler, TrackEvent};
use songbird::input::{AudioStream, Input, LiveInput, RawAdapter};
use songbird::tracks::{Track, TrackHandle};
use songbird::{Config, ConnectionInfo, Driver};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub fn shared() -> &'static Arc<Shared> {
    static S: OnceLock<Arc<Shared>> = OnceLock::new();
    S.get_or_init(Shared::new)
}

fn deaf_flag() -> &'static Arc<AtomicBool> {
    static D: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    D.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

static TRACK: Mutex<Option<TrackHandle>> = Mutex::new(None);

pub struct Voice {
    driver: Driver,
    _audio: AudioHandle,
}

fn driver_config() -> Config {
    Config::default()
        .decode_mode(DecodeMode::Decode(DecodeConfig::new(
            Channels::Mono,
            SampleRate::Hz48000,
        )))
        // 2 packets (40 ms) of jitter buffer instead of the default 5 (100 ms).
        .playout_buffer_length(NonZeroU8::new(2).unwrap())
        .playout_spike_length(2)
        .mix_mode(MixMode::Mono)
        .use_softclip(false)
}

/// Connect (or reconnect, e.g. after a channel move or server change).
pub async fn connect(cur: &mut Option<Voice>, info: ConnectionInfo) -> Result<(), String> {
    if let Some(v) = cur.as_mut() {
        return v.driver.connect(info).await.map_err(|e| e.to_string());
    }

    let mut driver = Driver::new(driver_config());
    driver.set_bitrate(Bitrate::Bits(64_000));
    let rx = Rx::default();
    for ev in [
        CoreEvent::VoiceTick,
        CoreEvent::SpeakingStateUpdate,
        CoreEvent::DriverConnect,
        CoreEvent::DriverReconnect,
        CoreEvent::DriverDisconnect,
    ] {
        driver.add_global_event(Event::Core(ev), rx.clone());
    }

    tracing::info!("connecting voice");
    driver.connect(info).await.map_err(|e| {
        tracing::warn!("voice connect failed: {e}");
        e.to_string()
    })?;
    tracing::info!("voice connected");

    let audio = audio::start(shared().clone(), deaf_flag().clone());
    let src = RawAdapter::new(MicSource::new(shared().clone()), audio::RATE as u32, 1);
    let input = Input::Live(
        LiveInput::Raw(AudioStream { input: Box::new(src) as Box<dyn songbird::input::core::io::MediaSource> }),
        None,
    );
    let handle = driver.play(Track::new(input).pause());
    for ev in [TrackEvent::Error, TrackEvent::Playable, TrackEvent::End] {
        let _ = handle.add_event(Event::Track(ev), MicTrackWatch);
    }
    *TRACK.lock() = Some(handle);

    *cur = Some(Voice { driver, _audio: audio });
    Ok(())
}

pub fn disconnect(cur: &mut Option<Voice>) {
    *TRACK.lock() = None;
    if let Some(mut v) = cur.take() {
        v.driver.leave();
    }
    let mut s = state();
    s.speaking.clear();
    s.self_talking = false;
    s.voice_ready = false;
    drop(s);
    state::changed();
}

/// Tell the DAVE layer who is in our channel (see the songbird patch).
pub fn set_known_users(ids: Vec<u64>) {
    if let Ok(mut w) = songbird::EXTRA_RECOGNIZED_USERS.write() {
        *w = ids;
    }
}

/// Reports what happens to our mic track (so a silent failure shows up).
struct MicTrackWatch;

#[async_trait]
impl EventHandler for MicTrackWatch {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(list) = ctx {
            for (st, _) in list.iter() {
                tracing::info!("mic track: {:?}", st.playing);
                if let songbird::tracks::PlayMode::Errored(e) = &st.playing {
                    state::set_status(format!("Mic error: {e}"));
                }
            }
        }
        None
    }
}

#[derive(Clone, Default)]
struct Rx(Arc<RxInner>);

#[derive(Default)]
struct RxInner {
    ssrc_user: Mutex<HashMap<u32, u64>>,
    last_heard: Mutex<HashMap<u64, Instant>>,
    shown: Mutex<HashSet<u64>>,
}

#[async_trait]
impl EventHandler for Rx {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(sp) => {
                if let Some(uid) = sp.user_id {
                    self.0.ssrc_user.lock().insert(sp.ssrc, uid.0);
                }
            }
            EventContext::VoiceTick(tick) => self.on_tick(tick),
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => {
                let mut s = state();
                s.voice_ready = true;
                s.status = "Voice connected".into();
                drop(s);
                state::changed();
            }
            EventContext::DriverDisconnect(d) => {
                let mut s = state();
                s.voice_ready = false;
                s.status = match &d.reason {
                    Some(r) => format!("Voice dropped: {r:?}"),
                    None => "Voice disconnected".into(),
                };
                s.speaking.clear();
                drop(s);
                state::changed();
            }
            _ => {}
        }
        None
    }
}

impl Rx {
    fn on_tick(&self, tick: &songbird::events::context_data::VoiceTick) {
        let mut mix = [0i32; 960];
        let mut any = false;
        let now = Instant::now();
        {
            let map = self.0.ssrc_user.lock();
            let mut heard = self.0.last_heard.lock();
            for (ssrc, data) in &tick.speaking {
                let Some(pcm) = &data.decoded_voice else { continue };
                if pcm.is_empty() {
                    continue;
                }
                any = true;
                let mut energy = 0f64;
                for (m, s) in mix.iter_mut().zip(pcm.iter()) {
                    *m += *s as i32;
                    energy += (*s as f64) * (*s as f64);
                }
                let rms = (energy / pcm.len() as f64).sqrt();
                if rms > 150.0 {
                    if let Some(uid) = map.get(ssrc) {
                        heard.insert(*uid, now);
                    }
                }
            }
            heard.retain(|_, t| now.duration_since(*t) < Duration::from_secs(5));

            let talking: HashSet<u64> = heard
                .iter()
                .filter(|(_, t)| now.duration_since(**t) < Duration::from_millis(250))
                .map(|(u, _)| *u)
                .collect();
            let mut shown = self.0.shown.lock();
            if *shown != talking {
                *shown = talking.clone();
                state().speaking = talking;
                state::changed();
            }
        }
        if any {
            let out: Vec<i16> = mix.iter().map(|s| (*s).clamp(-32768, 32767) as i16).collect();
            shared().push_out(&out);
        }
    }
}

/// Runs forever on its own thread: decides every 10 ms whether we should be
/// sending audio (mute, deafen, push-to-talk, voice activation).
pub fn transmit_loop(key_down: fn(u32) -> bool) {
    let sh = shared().clone();
    let mut sending = false;
    let mut shown_talking = false;
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let (live, muted, deaf, mode, key) = {
            let s = state();
            (
                s.joined.is_some() && s.voice_ready,
                s.muted,
                s.deafened,
                s.mode,
                s.ptt_key,
            )
        };
        deaf_flag().store(deaf, Ordering::Relaxed);
        let voice_now = sh.now_ms().saturating_sub(sh.last_voice_ms.load(Ordering::Relaxed)) < 400;
        let want = live
            && !muted
            && !deaf
            && match mode {
                MicMode::Open => true,
                MicMode::Voice => voice_now,
                MicMode::Ptt => key != 0 && key_down(key),
            };
        if want != sending {
            if let Some(h) = TRACK.lock().as_ref() {
                let r = if want {
                    sh.trim_mic_preroll();
                    h.play()
                } else {
                    h.pause()
                };
                if let Err(e) = r {
                    tracing::warn!("mic track control failed: {e:?}");
                    state::set_status("Mic stopped working - rejoin the channel");
                }
            }
            sending = want;
        }
        let talking = sending && (mode != MicMode::Open || voice_now);
        if talking != shown_talking {
            shown_talking = talking;
            state().self_talking = talking;
            state::changed();
        }
    }
}
