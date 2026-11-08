//! Main Discord gateway: login, server/channel list, who is in voice, and the
//! two events voice needs (VOICE_STATE_UPDATE, VOICE_SERVER_UPDATE).
//! Text messages, presences and everything else are ignored.

use crate::state::{self, state, Chan, Cmd, Guild, VoiceSt};
use crate::{golive, store, stream, voice};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use songbird::id::{ChannelId, GuildId, UserId};
use songbird::ConnectionInfo;
use std::num::NonZeroU64;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{interval_at, Instant};
use tokio_tungstenite::tungstenite::Message;

const GATEWAY: &str = "wss://gateway.discord.gg/?v=9&encoding=json";

fn id(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

#[derive(Default)]
struct Session {
    session_id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
    token_saved: bool,
}

/// What we know so far about the voice server we were assigned.
#[derive(Default)]
struct Pending {
    guild: u64,
    channel: u64,
    session_id: Option<String>,
    server: Option<(String, String)>, // (token, endpoint)
    /// (session, endpoint, token, channel) we last connected with
    connected_with: Option<(String, String, String, u64)>,
    /// Streamer to watch once voice is connected.
    pending_watch: Option<u64>,
}

enum End {
    Reconnect,
    Fresh,
    AuthFailed,
}

pub async fn run(mut rx: UnboundedReceiver<Cmd>) {
    let mut token = store::load_token();
    let mut sess = Session::default();
    let mut pend = Pending::default();
    let mut v: Option<voice::Voice> = None;

    loop {
        let Some(tok) = token.clone() else {
            {
                let mut s = state();
                s.logged_in = false;
                s.status = "Paste your Discord token, then press Log in".into();
            }
            state::changed();
            // Wait for a login; ignore everything else until then.
            while let Some(cmd) = rx.recv().await {
                if let Cmd::Login(t) = cmd {
                    let t = t.trim().trim_matches('"').to_string();
                    if !t.is_empty() {
                        token = Some(t);
                        sess = Session::default();
                        break;
                    }
                }
            }
            continue;
        };

        state::set_status("Connecting...");
        let end = session(&tok, &mut sess, &mut pend, &mut v, &mut rx).await;
        match end {
            End::Reconnect => tokio::time::sleep(Duration::from_secs(2)).await,
            End::Fresh => {
                sess.session_id = None;
                sess.resume_url = None;
                sess.seq = None;
                voice::disconnect(&mut v);
                pend = Pending::default();
                state().joined = None;
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            End::AuthFailed => {
                store::clear_token();
                token = None;
                sess = Session::default();
                voice::disconnect(&mut v);
                pend = Pending::default();
                state().joined = None;
                state::set_status("Token was rejected. Paste a new one.");
            }
        }
    }
}

async fn session(
    token: &str,
    sess: &mut Session,
    pend: &mut Pending,
    v: &mut Option<voice::Voice>,
    rx: &mut UnboundedReceiver<Cmd>,
) -> End {
    let url = sess.resume_url.clone().unwrap_or_else(|| GATEWAY.to_string());
    let (mut ws, _) = match tokio_tungstenite::connect_async(url.as_str()).await {
        Ok(x) => x,
        Err(e) => {
            state::set_status(format!("Can't reach Discord: {e}"));
            sess.resume_url = None;
            return End::Reconnect;
        }
    };

    // Hello
    let hb_ms = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                if v["op"] == 10 {
                    break v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
                }
            }
            Some(Ok(_)) => continue,
            _ => return End::Reconnect,
        }
    };

    let hello = if let (Some(sid), Some(seq)) = (&sess.session_id, sess.seq) {
        json!({"op": 6, "d": {"token": token, "session_id": sid, "seq": seq}})
    } else {
        json!({"op": 2, "d": {
            "token": token,
            "capabilities": 0,
            "properties": {"os": "Windows", "browser": "tinycord", "device": "tinycord"},
            "presence": {"status": "online", "since": 0, "activities": [], "afk": false},
            "compress": false
        }})
    };
    if ws.send(Message::Text(hello.to_string().into())).await.is_err() {
        return End::Reconnect;
    }

    let period = Duration::from_millis(hb_ms);
    let mut hb = interval_at(Instant::now() + period / 2, period);
    let mut acked = true;

    loop {
        let mut out: Vec<Value> = Vec::new();
        tokio::select! {
            msg = ws.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(frame))) => {
                        let code: u16 = frame.map(|f| f.code.into()).unwrap_or(1000);
                        return match code {
                            4004 => End::AuthFailed,
                            4007 | 4009 => End::Fresh,
                            4010..=4014 => { state::set_status(format!("Discord closed the connection ({code})")); End::AuthFailed }
                            _ => End::Reconnect,
                        };
                    }
                    Some(Ok(_)) => continue,
                    _ => return End::Reconnect,
                };
                let p: Value = match serde_json::from_str(text.as_str()) { Ok(p) => p, Err(_) => continue };
                drop(text);
                if let Some(s) = p["s"].as_u64() { sess.seq = Some(s); }
                match p["op"].as_u64().unwrap_or(99) {
                    0 => {
                        let t = p["t"].as_str().unwrap_or("");
                        if t == "READY" && !sess.token_saved {
                            store::save_token(token);
                            sess.token_saved = true;
                        }
                        dispatch(t, &p["d"], sess, pend, v, &mut out).await;
                    }
                    1 => out.push(json!({"op": 1, "d": sess.seq})),
                    7 => return End::Reconnect,
                    9 => return if p["d"].as_bool() == Some(true) { End::Reconnect } else { End::Fresh },
                    11 => acked = true,
                    _ => {}
                }
            }
            _ = hb.tick() => {
                if !acked {
                    return End::Reconnect; // zombie connection
                }
                acked = false;
                out.push(json!({"op": 1, "d": sess.seq}));
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { return End::Reconnect };
                command(cmd, pend, v, &mut out).await;
            }
        }
        for m in out {
            if ws.send(Message::Text(m.to_string().into())).await.is_err() {
                return End::Reconnect;
            }
        }
    }
}

fn voice_state_payload(guild: u64, channel: Option<u64>) -> Value {
    let s = state();
    json!({"op": 4, "d": {
        "guild_id": guild.to_string(),
        "channel_id": channel.map(|c| c.to_string()),
        "self_mute": s.muted || s.deafened,
        "self_deaf": s.deafened,
        "self_video": false
    }})
}

/// Ask Discord for names of people in voice whose names we don't know.
fn request_names(guild: u64, out: &mut Vec<Value>) {
    let s = state();
    let ids: Vec<String> = s
        .voice
        .iter()
        .filter(|(u, st)| st.guild == guild && !s.names.contains_key(u))
        .map(|(u, _)| u.to_string())
        .take(100)
        .collect();
    drop(s);
    if !ids.is_empty() {
        out.push(json!({"op": 8, "d": {"guild_id": guild.to_string(), "user_ids": ids, "presences": false}}));
    }
}

fn do_join(g: u64, c: u64, pend: &mut Pending, v: &mut Option<voice::Voice>, out: &mut Vec<Value>) {
    let current = state().joined;
    if let Some((og, _)) = current {
        if og != g {
            out.push(voice_state_payload(og, None));
            voice::disconnect(v);
            *pend = Pending::default();
        }
    }
    pend.guild = g;
    pend.channel = c;
    {
        let mut s = state();
        s.joined = Some((g, c));
        s.status = "Joining...".into();
    }
    state::changed();
    out.push(voice_state_payload(g, Some(c)));
    request_names(g, out);
}

async fn start_watch(g: u64, c: u64, u: u64, out: &mut Vec<Value>) {
    if let Some(old) = stream::current_key().await {
        out.push(json!({"op": 19, "d": {"stream_key": old}}));
    }
    let key = stream::key_for(g, c, u);
    let name = state().name_of(g, u);
    stream::begin(key.clone(), u, name).await;
    out.push(json!({"op": 20, "d": {"stream_key": key}}));
}

async fn stop_watch(out: &mut Vec<Value>) {
    if let Some(key) = stream::current_key().await {
        out.push(json!({"op": 19, "d": {"stream_key": key}}));
    }
    stream::stop().await;
}

async fn stop_live(out: &mut Vec<Value>) {
    if let Some(key) = golive::stop().await {
        out.push(json!({"op": 19, "d": {"stream_key": key}}));
    }
}

async fn go_live(cfg: state::LiveCfg, out: &mut Vec<Value>) {
    let (joined, ready, me, premium, boost) = {
        let s = state();
        let boost = s.joined.and_then(|(g, _)| s.guilds.iter().find(|x| x.id == g)).map(|g| g.boost).unwrap_or(0);
        (s.joined, s.voice_ready, s.me, s.premium, boost)
    };
    let Some((g, c)) = joined else {
        state::set_status("Join a voice channel first");
        return;
    };
    if !ready {
        state::set_status("Wait until you're connected to voice");
        return;
    }
    // Only what your account or this server allows.
    let cfg = golive::clamp(cfg, golive::allowed(premium, boost));
    if golive::live_in(g, c).await {
        golive::reconfigure(cfg).await;
        return;
    }
    stop_live(out).await;
    let key = stream::key_for(g, c, me);
    golive::begin(key.clone(), g, c, cfg).await;
    out.push(json!({"op": 18, "d": {
        "type": "guild", "guild_id": g.to_string(), "channel_id": c.to_string(), "preferred_region": null
    }}));
    out.push(json!({"op": 22, "d": {"stream_key": key, "paused": false}}));
}

async fn command(cmd: Cmd, pend: &mut Pending, v: &mut Option<voice::Voice>, out: &mut Vec<Value>) {
    match cmd {
        Cmd::Login(_) => {}
        Cmd::ViewGuild(g) => {
            state().selected_guild = g;
            store::save_settings();
            request_names(g, out);
        }
        Cmd::Join(g, c) => {
            if state().joined != Some((g, c)) {
                stop_watch(out).await;
                stop_live(out).await;
            }
            do_join(g, c, pend, v, out);
        }
        Cmd::Watch(_, _, u) if u == state().me => {}
        Cmd::Watch(g, c, u) => {
            let ready = {
                let s = state();
                s.joined == Some((g, c)) && s.voice_ready
            };
            if ready {
                start_watch(g, c, u, out).await;
            } else {
                // You have to be in the channel to watch: join, then watch.
                stop_watch(out).await;
                do_join(g, c, pend, v, out);
                pend.pending_watch = Some(u);
            }
        }
        Cmd::StopWatch => stop_watch(out).await,
        Cmd::GoLive(cfg) => go_live(cfg, out).await,
        Cmd::StopLive => stop_live(out).await,
        Cmd::Leave => {
            stop_watch(out).await;
            stop_live(out).await;
            let current = state().joined.take();
            if let Some((g, _)) = current {
                out.push(voice_state_payload(g, None));
            }
            voice::disconnect(v);
            *pend = Pending::default();
            state::set_status("Left voice");
        }
        Cmd::RestartAudio => voice::restart_audio(v),
        Cmd::UpdateSelf => {
            let current = state().joined;
            if let Some((g, c)) = current {
                out.push(voice_state_payload(g, Some(c)));
            }
            state::changed();
        }
    }
}

fn learn_member(guild: u64, m: &Value) {
    let user = &m["user"];
    let uid = id(&user["id"]);
    if uid == 0 {
        return;
    }
    let mut s = state();
    let name = user["global_name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or_else(|| user["username"].as_str());
    if let Some(n) = name {
        s.names.insert(uid, n.to_string());
    }
    if let Some(nick) = m["nick"].as_str().filter(|n| !n.is_empty()) {
        s.nicks.insert((guild, uid), nick.to_string());
    }
}

fn learn_voice_state(guild: u64, vs: &Value) {
    let uid = id(&vs["user_id"]);
    if uid == 0 {
        return;
    }
    let ch = id(&vs["channel_id"]);
    let mut s = state();
    if ch == 0 {
        s.voice.remove(&uid);
    } else {
        s.voice.insert(
            uid,
            VoiceSt {
                guild,
                channel: ch,
                mute: vs["self_mute"].as_bool().unwrap_or(false) || vs["mute"].as_bool().unwrap_or(false),
                deaf: vs["self_deaf"].as_bool().unwrap_or(false) || vs["deaf"].as_bool().unwrap_or(false),
                streaming: vs["self_stream"].as_bool().unwrap_or(false),
            },
        );
    }
}

fn parse_guild(g: &Value) -> Option<Guild> {
    if g["unavailable"].as_bool() == Some(true) {
        return None;
    }
    let gid = id(&g["id"]);
    let name = g["name"]
        .as_str()
        .or_else(|| g["properties"]["name"].as_str())
        .unwrap_or("?")
        .to_string();
    let mut voice: Vec<Chan> = g["channels"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter(|c| c["type"].as_u64() == Some(2))
                .map(|c| Chan {
                    id: id(&c["id"]),
                    name: c["name"].as_str().unwrap_or("?").to_string(),
                    position: c["position"].as_i64().unwrap_or(0),
                    bitrate: c["bitrate"].as_u64().unwrap_or(64_000) as u32,
                })
                .collect()
        })
        .unwrap_or_default();
    voice.sort_by_key(|c| c.position);
    if let Some(vss) = g["voice_states"].as_array() {
        for vs in vss {
            learn_voice_state(gid, vs);
        }
    }
    if let Some(ms) = g["members"].as_array() {
        for m in ms {
            learn_member(gid, m);
        }
    }
    let boost = g["premium_tier"]
        .as_u64()
        .or_else(|| g["properties"]["premium_tier"].as_u64())
        .unwrap_or(0)
        .min(3) as u8;
    Some(Guild { id: gid, name, voice, boost })
}

fn upsert_guild(gd: Guild) {
    let mut s = state();
    if let Some(slot) = s.guilds.iter_mut().find(|x| x.id == gd.id) {
        *slot = gd;
    } else {
        s.guilds.push(gd);
    }
}

fn update_known_users(pend: &Pending) {
    let me = state().me;
    let ids: Vec<u64> = {
        let s = state();
        s.voice
            .iter()
            .filter(|(u, st)| **u != me && st.guild == pend.guild && st.channel == pend.channel)
            .map(|(u, _)| *u)
            .collect()
    };
    voice::set_known_users(ids);
}

async fn dispatch(
    t: &str,
    d: &Value,
    sess: &mut Session,
    pend: &mut Pending,
    v: &mut Option<voice::Voice>,
    out: &mut Vec<Value>,
) {
    match t {
        "READY" => {
            sess.session_id = d["session_id"].as_str().map(String::from);
            sess.resume_url = d["resume_gateway_url"]
                .as_str()
                .map(|u| format!("{u}/?v=9&encoding=json"));
            let me = id(&d["user"]["id"]);
            let me_name = d["user"]["global_name"]
                .as_str()
                .or_else(|| d["user"]["username"].as_str())
                .unwrap_or("you")
                .to_string();
            {
                let mut s = state();
                s.me = me;
                s.premium = d["user"]["premium_type"].as_u64().unwrap_or(0).min(3) as u8;
                s.names.insert(me, me_name.clone());
                s.guilds.clear();
                s.voice.clear();
            }
            if let Some(us) = d["users"].as_array() {
                for u in us {
                    learn_member(0, &json!({ "user": u }));
                }
            }
            if let Some(gs) = d["guilds"].as_array() {
                for g in gs {
                    if let Some(gd) = parse_guild(g) {
                        upsert_guild(gd);
                    }
                }
            }
            {
                let mut s = state();
                s.guilds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                s.logged_in = true;
                s.status = format!("Logged in as {me_name}");
            }
            state::changed();
            let g = state().selected_guild;
            if g != 0 {
                request_names(g, out);
            }
        }
        "RESUMED" => state::set_status("Reconnected"),
        "READY_SUPPLEMENTAL" => {
            if let Some(gs) = d["guilds"].as_array() {
                for g in gs {
                    let gid = id(&g["id"]);
                    if let Some(vss) = g["voice_states"].as_array() {
                        for vs in vss {
                            learn_voice_state(gid, vs);
                        }
                    }
                }
            }
            state::changed();
        }
        "GUILD_CREATE" => {
            if let Some(gd) = parse_guild(d) {
                upsert_guild(gd);
                state()
                    .guilds
                    .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                state::changed();
            }
        }
        "GUILD_DELETE" => {
            let gid = id(&d["id"]);
            let mut s = state();
            s.guilds.retain(|g| g.id != gid);
            s.voice.retain(|_, st| st.guild != gid);
            drop(s);
            state::changed();
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
            let gid = id(&d["guild_id"]);
            let cid = id(&d["id"]);
            let mut s = state();
            if let Some(g) = s.guilds.iter_mut().find(|g| g.id == gid) {
                g.voice.retain(|c| c.id != cid);
                if t != "CHANNEL_DELETE" && d["type"].as_u64() == Some(2) {
                    g.voice.push(Chan {
                        id: cid,
                        name: d["name"].as_str().unwrap_or("?").to_string(),
                        position: d["position"].as_i64().unwrap_or(0),
                        bitrate: d["bitrate"].as_u64().unwrap_or(64_000) as u32,
                    });
                    g.voice.sort_by_key(|c| c.position);
                }
            }
            drop(s);
            state::changed();
        }
        "GUILD_MEMBERS_CHUNK" => {
            let gid = id(&d["guild_id"]);
            if let Some(ms) = d["members"].as_array() {
                for m in ms {
                    learn_member(gid, m);
                }
            }
            state::changed();
        }
        "VOICE_STATE_UPDATE" => {
            let gid = id(&d["guild_id"]);
            if gid == 0 {
                return; // DM calls are not supported
            }
            if d["member"].is_object() {
                learn_member(gid, &d["member"]);
            }
            learn_voice_state(gid, d);
            let uid = id(&d["user_id"]);
            let me = state().me;
            if uid == me {
                let ch = id(&d["channel_id"]);
                if ch == 0 {
                    // We left, or were disconnected by someone.
                    if state().joined.map(|(g, _)| g) == Some(gid) {
                        stream::stop().await;
                        golive::stop().await;
                        voice::disconnect(v);
                        *pend = Pending::default();
                        state().joined = None;
                        state::set_status("Disconnected from voice");
                    }
                } else if state().joined.is_some() {
                    if state().joined != Some((gid, ch)) {
                        // Moved to another channel: a stream there isn't ours.
                        stop_watch(out).await;
                        stop_live(out).await;
                    }
                    pend.guild = gid;
                    pend.channel = ch;
                    pend.session_id = d["session_id"].as_str().map(String::from);
                    state().joined = Some((gid, ch));
                    try_voice(pend, v).await;
                    watch_if_pending(pend, out).await;
                }
            }
            update_known_users(pend);
            state::changed();
        }
        "VOICE_SERVER_UPDATE" => {
            let gid = id(&d["guild_id"]);
            if gid != pend.guild {
                return;
            }
            match (d["token"].as_str(), d["endpoint"].as_str()) {
                (Some(tok), Some(ep)) => {
                    pend.server = Some((tok.to_string(), ep.to_string()));
                    try_voice(pend, v).await;
                    watch_if_pending(pend, out).await;
                }
                _ => state::set_status("Waiting for a voice server..."),
            }
        }
        "STREAM_CREATE" | "STREAM_SERVER_UPDATE" | "STREAM_UPDATE" | "STREAM_DELETE" => {
            tracing::info!("{t}: {}", redact(d));
            stream::on_event(t, d, pend.session_id.clone()).await;
            golive::on_event(t, d, pend.session_id.clone()).await;
        }
        _ => {}
    }
}

/// The event without its token (for the log file).
fn redact(d: &Value) -> Value {
    let mut d = d.clone();
    if d.get("token").is_some() {
        d["token"] = json!("(hidden)");
    }
    d
}

async fn watch_if_pending(pend: &mut Pending, out: &mut Vec<Value>) {
    if !state().voice_ready {
        return;
    }
    if let Some(u) = pend.pending_watch.take() {
        start_watch(pend.guild, pend.channel, u, out).await;
    }
}

async fn try_voice(pend: &mut Pending, v: &mut Option<voice::Voice>) {
    let (Some(sid), Some((tok, ep))) = (pend.session_id.clone(), pend.server.clone()) else {
        return;
    };
    let key = (sid.clone(), ep.clone(), tok.clone(), pend.channel);
    if pend.connected_with.as_ref() == Some(&key) {
        return; // e.g. just a mute toggle
    }
    let (Some(g), Some(c)) = (NonZeroU64::new(pend.guild), NonZeroU64::new(pend.channel)) else {
        return;
    };
    let Some(me) = NonZeroU64::new(state().me) else { return };
    update_known_users(pend);
    let info = ConnectionInfo {
        channel_id: ChannelId::from(c),
        endpoint: ep,
        guild_id: GuildId::from(g),
        session_id: sid,
        token: tok,
        user_id: UserId::from(me),
    };
    let bitrate = {
        let s = state();
        s.guilds
            .iter()
            .find(|g| g.id == pend.guild)
            .and_then(|g| g.voice.iter().find(|c| c.id == pend.channel))
            .map(|c| c.bitrate)
            .unwrap_or(64_000)
    };
    state::set_status("Connecting voice...");
    match voice::connect(v, info, bitrate).await {
        Ok(()) => {
            pend.connected_with = Some(key);
            let mut s = state();
            s.voice_ready = true;
            s.status = "Voice connected".into();
            drop(s);
            state::changed();
        }
        Err(e) => state::set_status(format!("Voice failed: {e}")),
    }
}
