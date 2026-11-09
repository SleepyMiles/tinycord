//! Text chat: server channels and DMs, opened on demand like a stream.
//!
//! Nothing is fetched or kept until you open a chat. The open chat keeps at
//! most `KEEP` messages as plain text (no images), and closing the window
//! frees them and the web connection. Pings (mentions and DMs) are noticed
//! from the messages Discord already sends over the main connection, so they
//! work with the chat window closed.

use crate::state::{self, state, Chat, Msg, NotifySettings, Ping};
use serde_json::{json, Value};
use std::sync::Mutex;

const API: &str = "https://discord.com/api/v9";
const KEEP: usize = 200;

static TOKEN: Mutex<Option<String>> = Mutex::new(None);
/// The web client, only while a chat is open.
static CLIENT: Mutex<Option<reqwest::Client>> = Mutex::new(None);

pub fn set_token(t: Option<String>) {
    *TOKEN.lock().unwrap() = t;
}

fn id(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()).unwrap_or(0)
}

fn client() -> Option<(reqwest::Client, String)> {
    let token = TOKEN.lock().unwrap().clone()?;
    let mut c = CLIENT.lock().unwrap();
    if c.is_none() {
        *c = reqwest::Client::builder()
            .user_agent(concat!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) tinycord/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(1)
            .build()
            .ok();
    }
    Some((c.clone()?, token))
}

fn set_chat_status(channel: u64, s: impl Into<String>) {
    let mut st = state();
    if let Some(c) = st.chat.as_mut().filter(|c| c.channel == channel) {
        c.status = s.into();
        c.version += 1;
    }
    drop(st);
    state::changed();
}

fn http_error(code: u16, body: &str) -> String {
    match code {
        401 => "Discord rejected the login (log in again)".into(),
        403 => "You don't have access to this channel".into(),
        404 => "Channel not found".into(),
        429 => {
            let wait = serde_json::from_str::<Value>(body).ok().and_then(|v| v["retry_after"].as_f64()).unwrap_or(1.0);
            format!("Slow down: try again in {wait:.0} s")
        }
        _ => format!("Discord said {code}"),
    }
}

/// Open a channel's chat: show it right away, then load recent messages.
pub fn open(guild: u64, channel: u64) {
    {
        let mut s = state();
        let version = s.chat.as_ref().map(|c| c.version + 1).unwrap_or(0);
        s.chat = Some(Chat { guild, channel, msgs: Vec::new(), status: "Loading...".into(), version });
        // Looking at it counts as seeing its pings.
        s.pings.retain(|p| p.channel != channel);
    }
    state::changed();
    tokio::spawn(async move {
        let Some((http, token)) = client() else { return };
        let url = format!("{API}/channels/{channel}/messages?limit=50");
        let res = http.get(&url).header("Authorization", &token).send().await;
        let (code, body) = match res {
            Ok(r) => (r.status().as_u16(), r.text().await.unwrap_or_default()),
            Err(e) => {
                tracing::warn!("chat load: {e}");
                set_chat_status(channel, "Couldn't reach Discord");
                return;
            }
        };
        if code != 200 {
            set_chat_status(channel, http_error(code, &body));
            return;
        }
        let list: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
        let mut msgs: Vec<Msg> = list.iter().rev().map(render).collect();
        {
            let mut s = state();
            if let Some(c) = s.chat.as_mut().filter(|c| c.channel == channel) {
                // Keep anything that arrived live while loading.
                let have: Vec<u64> = msgs.iter().map(|m| m.id).collect();
                msgs.extend(c.msgs.drain(..).filter(|m| !have.contains(&m.id)));
                c.msgs = msgs;
                c.status.clear();
                c.version += 1;
            }
        }
        state::changed();
    });
}

pub fn close() {
    state().chat = None;
    *CLIENT.lock().unwrap() = None;
    state::changed();
}

pub fn send(channel: u64, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let Some((http, token)) = client() else { return };
        let nonce = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
            as u64
            - 1_420_070_400_000)
            << 22;
        let body = json!({"content": text, "nonce": nonce.to_string(), "tts": false, "flags": 0});
        let res = http
            .post(format!("{API}/channels/{channel}/messages"))
            .header("Authorization", &token)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {} // it comes back as MESSAGE_CREATE
            Ok(r) => {
                let code = r.status().as_u16();
                let b = r.text().await.unwrap_or_default();
                let why = if b.contains("captcha") {
                    "Discord wants a captcha. Send one message from the Discord app or website, then try again."
                        .to_string()
                } else {
                    format!("Couldn't send: {}", http_error(code, &b))
                };
                set_chat_status(channel, why);
            }
            Err(e) => {
                tracing::warn!("chat send: {e}");
                set_chat_status(channel, "Couldn't send: no connection");
            }
        }
    });
}

/// MESSAGE_CREATE / UPDATE / DELETE from the main connection.
pub fn on_event(t: &str, d: &Value) {
    let channel = id(&d["channel_id"]);
    let mut changed = false;
    // Render before taking the state lock (rendering looks up names in it).
    let rendered = (t == "MESSAGE_CREATE" || d.get("content").is_some()).then(|| render(d));
    {
        let mut s = state();
        let me = s.me;
        if t == "MESSAGE_CREATE" {
            // Newest DMs first.
            if let Some(i) = s.dms.iter().position(|x| x.id == channel) {
                let mut dm = s.dms.remove(i);
                dm.last = id(&d["id"]);
                s.dms.insert(0, dm);
            }
            let open = s.chat.as_ref().is_some_and(|c| c.channel == channel);
            if !open && is_ping(d, me, &s.notify) {
                let ping = make_ping(d, &s, rendered.as_ref());
                s.pings.push(ping);
                if s.pings.len() > 50 {
                    s.pings.remove(0);
                }
                s.ping_seq += 1;
                changed = true;
            }
        }
        if let Some(c) = s.chat.as_mut().filter(|c| c.channel == channel) {
            let mid = id(&d["id"]);
            match t {
                "MESSAGE_CREATE" => {
                    let Some(m) = rendered.clone() else { return };
                    if !c.msgs.iter().any(|x| x.id == m.id) {
                        c.msgs.push(m);
                        if c.msgs.len() > KEEP {
                            let cut = c.msgs.len() - KEEP;
                            c.msgs.drain(..cut);
                        }
                    }
                }
                // Updates can be partial (an embed loading): only redo
                // messages whose text is in the update.
                "MESSAGE_UPDATE" if d.get("content").is_some() => {
                    if let (Some(x), Some(m)) = (c.msgs.iter_mut().find(|x| x.id == mid), rendered.clone()) {
                        let author = x.author.clone();
                        *x = m;
                        if x.author == "?" {
                            x.author = author;
                        }
                    }
                }
                "MESSAGE_DELETE" => c.msgs.retain(|x| x.id != mid),
                _ => return,
            }
            c.status.clear();
            c.version += 1;
            changed = true;
        }
    }
    if changed {
        state::changed();
    }
}

/// Does this message deserve a notification? DMs and mentions of you
/// (including @everyone/@here), unless you muted that server or channel in
/// Discord, or set the server to "Nothing".
pub fn is_ping(d: &Value, me: u64, n: &NotifySettings) -> bool {
    if id(&d["author"]["id"]) == me || me == 0 {
        return false;
    }
    let guild = id(&d["guild_id"]);
    let channel = id(&d["channel_id"]);
    if n.muted_channels.contains(&channel) {
        return false;
    }
    if guild == 0 {
        return true; // a DM
    }
    if n.muted_guilds.contains(&guild) || n.silent_guilds.contains(&guild) {
        return false;
    }
    let mentioned = d["mentions"].as_array().is_some_and(|m| m.iter().any(|u| id(&u["id"]) == me));
    let everyone = d["mention_everyone"].as_bool() == Some(true) && !n.no_everyone.contains(&guild);
    mentioned || everyone
}

/// `s` is the locked state, so nothing in here may lock it again.
fn make_ping(d: &Value, s: &state::State, rendered: Option<&Msg>) -> Ping {
    let guild = id(&d["guild_id"]);
    let channel = id(&d["channel_id"]);
    let author = author_name(d);
    let title = if guild == 0 {
        author
    } else {
        let g = s.guilds.iter().find(|g| g.id == guild);
        let ch = g.and_then(|g| g.text.iter().find(|c| c.id == channel)).map(|c| c.name.as_str()).unwrap_or("?");
        format!("{author} in #{ch} ({})", g.map(|g| g.name.as_str()).unwrap_or("?"))
    };
    let mut text = rendered.map(|m| m.text.replace('\n', " ")).unwrap_or_default();
    if text.chars().count() > 200 {
        text = text.chars().take(197).collect::<String>() + "...";
    }
    Ping { guild, channel, title, text }
}

fn author_name(d: &Value) -> String {
    d["member"]["nick"]
        .as_str()
        .or_else(|| d["author"]["global_name"].as_str())
        .or_else(|| d["author"]["username"].as_str())
        .unwrap_or("?")
        .to_string()
}

/// A message as one or more plain-text lines.
pub fn render(d: &Value) -> Msg {
    let mid = id(&d["id"]);
    let mut text = clean(d["content"].as_str().unwrap_or(""), d);
    if let Some(r) = d["referenced_message"].as_object() {
        let who = Value::Object(r.clone());
        text = format!("(reply to {}) {text}", author_name(&who));
    }
    let mut files = Vec::new();
    for a in d["attachments"].as_array().into_iter().flatten() {
        let name = a["filename"].as_str().unwrap_or("?");
        let image = a["content_type"].as_str().is_some_and(|t| t.starts_with("image/"))
            || [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|x| name.to_lowercase().ends_with(x));
        let label = format!("[{}: {name}]", if image { "image" } else { "file" });
        text += &format!(" {label}");
        if let Some(url) = a["url"].as_str() {
            files.push((label, url.to_string()));
        }
    }
    for e in d["embeds"].as_array().into_iter().flatten() {
        let kind = e["type"].as_str().unwrap_or("");
        // GIFs (Tenor etc.) and image links: open the original link.
        let link = match kind {
            "gifv" | "image" => e["url"].as_str(),
            _ => e["image"]["url"].as_str(),
        };
        if let Some(url) = link {
            let label = if kind == "gifv" { "[gif]".to_string() } else { "[image]".to_string() };
            text += &format!(" {label}");
            files.push((label, url.to_string()));
        } else if let Some(t) = e["title"].as_str().or_else(|| e["description"].as_str()) {
            let t: String = t.chars().take(80).collect();
            text += &format!(" [{t}]");
        }
    }
    for st in d["sticker_items"].as_array().into_iter().flatten() {
        text += &format!(" [sticker: {}]", st["name"].as_str().unwrap_or("?"));
    }
    let text = match (text.trim().is_empty(), d["type"].as_u64().unwrap_or(0)) {
        (true, 7) => "joined the server".to_string(),
        (true, 8..=11) => "boosted the server".to_string(),
        (true, 6) => "pinned a message".to_string(),
        (true, _) => "[message]".to_string(),
        (false, _) => text.trim().to_string(),
    };
    Msg { id: mid, author: author_name(d), text, files }
}

/// Discord's markup to readable text: <@id> -> @name, <#id> -> #channel,
/// <:emoji:id> -> :emoji:, <t:unix> -> a date.
fn clean(content: &str, d: &Value) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find('<') {
        out += &rest[..start];
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            out += after;
            return out;
        };
        let inner = &after[1..end];
        out += &tag(inner, d).unwrap_or_else(|| format!("<{inner}>"));
        rest = &after[end + 1..];
    }
    out += rest;
    out
}

fn tag(inner: &str, d: &Value) -> Option<String> {
    if let Some(u) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        if let Some(_role) = u.strip_prefix('&') {
            return Some("@role".into());
        }
        let uid: u64 = u.parse().ok()?;
        let from_msg = d["mentions"].as_array().and_then(|m| m.iter().find(|x| id(&x["id"]) == uid)).map(|x| {
            x["member"]["nick"]
                .as_str()
                .or_else(|| x["global_name"].as_str())
                .or_else(|| x["username"].as_str())
                .unwrap_or("?")
                .to_string()
        });
        let name = from_msg.or_else(|| state().names.get(&uid).cloned()).unwrap_or_else(|| "someone".into());
        return Some(format!("@{name}"));
    }
    if let Some(c) = inner.strip_prefix('#') {
        let cid: u64 = c.parse().ok()?;
        let s = state();
        let name = s.guilds.iter().flat_map(|g| g.text.iter()).find(|x| x.id == cid).map(|x| x.name.clone());
        return Some(format!("#{}", name.unwrap_or_else(|| "channel".into())));
    }
    if let Some(e) = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':')) {
        let name = e.split(':').next()?;
        return Some(format!(":{name}:"));
    }
    if let Some(t) = inner.strip_prefix("t:") {
        let secs: i64 = t.split(':').next()?.parse().ok()?;
        return Some(date_time(secs * 1000));
    }
    None
}

/// Message time from its id (Discord ids carry the time they were made).
pub fn time_of(message_id: u64) -> String {
    let ms = (message_id >> 22) as i64 + 1_420_070_400_000;
    let local = ms + utc_offset_minutes() as i64 * 60_000;
    let mins = local.div_euclid(60_000);
    format!("{:02}:{:02}", mins.div_euclid(60).rem_euclid(24), mins.rem_euclid(60))
}

fn date_time(ms_utc: i64) -> String {
    let ms = ms_utc + utc_offset_minutes() as i64 * 60_000;
    let days = ms.div_euclid(86_400_000);
    let mins = ms.rem_euclid(86_400_000) / 60_000;
    // Days since 1970-01-01 to a date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year}-{month:02}-{day:02} {:02}:{:02}", mins / 60, mins % 60)
}

/// Your time zone's offset from UTC, in minutes.
#[cfg(windows)]
fn utc_offset_minutes() -> i32 {
    use windows_sys::Win32::System::Time::*;
    static OFFSET: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| unsafe {
        let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tz);
        let extra = if r == 2 { tz.DaylightBias } else { 0 }; // TIME_ZONE_ID_DAYLIGHT
        -(tz.Bias + extra)
    })
}

#[cfg(not(windows))]
fn utc_offset_minutes() -> i32 {
    0
}

/// READY's notification settings (muted servers/channels, @everyone).
pub fn parse_settings(v: &Value) -> NotifySettings {
    let mut n = NotifySettings::default();
    let entries = v["entries"].as_array().or_else(|| v.as_array());
    for e in entries.into_iter().flatten() {
        let g = id(&e["guild_id"]);
        if e["muted"].as_bool() == Some(true) && g != 0 {
            n.muted_guilds.insert(g);
        }
        if e["suppress_everyone"].as_bool() == Some(true) {
            n.no_everyone.insert(g);
        }
        if e["message_notifications"].as_u64() == Some(2) {
            n.silent_guilds.insert(g);
        }
        for c in e["channel_overrides"].as_array().into_iter().flatten() {
            if c["muted"].as_bool() == Some(true) {
                n.muted_channels.insert(id(&c["channel_id"]));
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_to_text() {
        let d = json!({
            "id": "1", "content": "hi <@42> see <:pog:123> and <t:0>",
            "author": {"id": "7", "username": "bob"},
            "mentions": [{"id": "42", "username": "miles", "global_name": "Miles"}],
            "attachments": [{"filename": "a.png", "url": "https://cdn.example/a.png"}],
        });
        let m = render(&d);
        assert_eq!(m.author, "bob");
        assert!(m.text.starts_with("hi @Miles see :pog: and 1970-01-01 00:00"), "{}", m.text);
        assert!(m.text.ends_with("[image: a.png]"), "{}", m.text);
        assert_eq!(m.files, vec![("[image: a.png]".to_string(), "https://cdn.example/a.png".to_string())]);
    }

    #[test]
    fn pings() {
        let me = 42;
        let mut n = NotifySettings::default();
        let mention = json!({"author": {"id": "7"}, "guild_id": "5", "channel_id": "9", "mentions": [{"id": "42"}]});
        let plain = json!({"author": {"id": "7"}, "guild_id": "5", "channel_id": "9", "mentions": []});
        let dm = json!({"author": {"id": "7"}, "channel_id": "8"});
        let mine = json!({"author": {"id": "42"}, "channel_id": "8"});
        let everyone = json!({"author": {"id": "7"}, "guild_id": "5", "channel_id": "9", "mention_everyone": true});
        assert!(is_ping(&mention, me, &n));
        assert!(!is_ping(&plain, me, &n));
        assert!(is_ping(&dm, me, &n));
        assert!(!is_ping(&mine, me, &n));
        assert!(is_ping(&everyone, me, &n));
        n.no_everyone.insert(5);
        assert!(!is_ping(&everyone, me, &n));
        n.muted_guilds.insert(5);
        assert!(!is_ping(&mention, me, &n));
        n.muted_channels.insert(8);
        assert!(!is_ping(&dm, me, &n));
        let s = parse_settings(&json!({"entries": [
            {"guild_id": "5", "muted": true, "suppress_everyone": true, "message_notifications": 2,
             "channel_overrides": [{"channel_id": "9", "muted": true}]}
        ]}));
        assert!(s.muted_guilds.contains(&5) && s.no_everyone.contains(&5) && s.silent_guilds.contains(&5));
        assert!(s.muted_channels.contains(&9));
    }

    #[test]
    fn times() {
        // 2015-01-01 00:00 UTC is Discord's epoch (id 0).
        assert_eq!(date_time(1_420_070_400_000), "2015-01-01 00:00");
        assert_eq!(date_time(1_730_000_000_000), "2024-10-27 03:33");
        assert_eq!(time_of(0), "00:00");
    }
}
