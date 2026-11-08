//! Shared app state. The network side writes, the UI reads and redraws.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug)]
pub struct Chan {
    pub id: u64,
    pub name: String,
    pub position: i64,
    /// Channel bitrate in bits/s set by the server (Discord default 64000).
    pub bitrate: u32,
}

#[derive(Clone, Debug)]
pub struct Guild {
    pub id: u64,
    pub name: String,
    pub voice: Vec<Chan>,
    /// Server boost level 0-3 (raises Go Live quality for everyone).
    pub boost: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct VoiceSt {
    pub guild: u64,
    pub channel: u64,
    pub mute: bool,
    pub deaf: bool,
    /// Sharing their screen with Go Live.
    pub streaming: bool,
}

/// The stream we're watching.
#[derive(Clone, Debug)]
pub struct Watching {
    pub user: u64,
    pub name: String,
    pub status: String,
}

/// Go Live settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveCfg {
    /// Index into the monitor list (usize::MAX = the main screen).
    pub monitor: usize,
    /// Picture height (480, 720, 1080, 1440), 0 = your screen's own size.
    pub height: u32,
    pub fps: u32,
    /// Also send what your PC is playing.
    pub sound: bool,
}

impl Default for LiveCfg {
    fn default() -> Self {
        Self { monitor: usize::MAX, height: 720, fps: 30, sound: true }
    }
}

/// Our own stream while it's on.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub guild: u64,
    pub channel: u64,
    pub status: String,
    pub viewers: usize,
    /// What's actually going out: picture size, frames/s, kbit/s.
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub kbps: u32,
    /// A problem with PC sound (the picture still goes out).
    pub sound_note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicMode {
    Open,
    Voice,
    Ptt,
}

pub struct State {
    pub status: String,
    pub logged_in: bool,
    pub me: u64,
    pub guilds: Vec<Guild>,
    /// user id -> where they are in voice
    pub voice: HashMap<u64, VoiceSt>,
    /// user id -> display name
    pub names: HashMap<u64, String>,
    /// (guild, user) -> server nickname
    pub nicks: HashMap<(u64, u64), String>,
    /// the channel we asked to be in
    pub joined: Option<(u64, u64)>,
    pub watching: Option<Watching>,
    /// Our Go Live stream, while it's on.
    pub live: Option<Live>,
    pub live_cfg: LiveCfg,
    /// Nitro: 0 none, 1 Classic, 2 Nitro, 3 Basic.
    pub premium: u8,
    pub voice_ready: bool,
    pub speaking: HashSet<u64>,
    pub self_talking: bool,
    pub muted: bool,
    pub deafened: bool,
    pub mode: MicMode,
    pub ptt_key: u32,
    pub capturing_key: bool,
    /// 0 = off, 1 top-left, 2 top-right, 3 bottom-left, 4 bottom-right,
    /// 5 left-middle (OVERLAY_MAX)
    pub overlay: u8,
    /// Main window stays on top of other windows.
    pub keep_on_top: bool,
    pub overlay_speakers_only: bool,
    pub selected_guild: u64,
    /// Chosen devices by name; None = Windows default.
    pub mic_device: Option<String>,
    pub spk_device: Option<String>,
    /// True once a mic choice was saved (so we only auto-pick Krisp once).
    pub mic_chosen: bool,
    pub input_devices: Vec<String>,
    pub output_devices: Vec<String>,
}

impl State {
    pub fn name_of(&self, guild: u64, user: u64) -> String {
        if let Some(n) = self.nicks.get(&(guild, user)) {
            return n.clone();
        }
        if let Some(n) = self.names.get(&user) {
            return n.clone();
        }
        format!("user {}", user % 100000)
    }

    /// Users in a voice channel, sorted by name.
    pub fn members_of(&self, guild: u64, channel: u64) -> Vec<(u64, String, VoiceSt)> {
        let mut v: Vec<_> = self
            .voice
            .iter()
            .filter(|(_, s)| s.guild == guild && s.channel == channel)
            .map(|(u, s)| (*u, self.name_of(guild, *u), *s))
            .collect();
        v.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        v
    }

    pub fn is_speaking(&self, user: u64) -> bool {
        if user == self.me {
            self.self_talking
        } else {
            self.speaking.contains(&user)
        }
    }
}

pub fn state() -> parking_lot::MutexGuard<'static, State> {
    cell().lock()
}

fn cell() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            status: "Starting...".into(),
            logged_in: false,
            me: 0,
            guilds: Vec::new(),
            voice: HashMap::new(),
            names: HashMap::new(),
            nicks: HashMap::new(),
            joined: None,
            watching: None,
            live: None,
            live_cfg: LiveCfg::default(),
            premium: 0,
            voice_ready: false,
            speaking: HashSet::new(),
            self_talking: false,
            muted: false,
            deafened: false,
            mode: MicMode::Voice,
            ptt_key: 0,
            capturing_key: false,
            overlay: 2,
            keep_on_top: false,
            overlay_speakers_only: false,
            selected_guild: 0,
            mic_device: None,
            spk_device: None,
            mic_chosen: false,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
        })
    })
}

/// Commands from the UI to the network task.
#[derive(Debug)]
pub enum Cmd {
    Login(String),
    ViewGuild(u64),
    Join(u64, u64),
    Leave,
    UpdateSelf,
    RestartAudio,
    /// Watch (guild, channel, streamer)'s Go Live stream.
    Watch(u64, u64, u64),
    StopWatch,
    /// Share your screen in the channel you're in (or apply new settings).
    GoLive(LiveCfg),
    StopLive,
    /// Leave voice, disconnect and forget the saved token.
    Logout,
}

/// Highest overlay position number (see `State::overlay`).
pub const OVERLAY_MAX: u8 = 5;

static CMD: OnceLock<UnboundedSender<Cmd>> = OnceLock::new();

pub fn set_cmd_sender(tx: UnboundedSender<Cmd>) {
    let _ = CMD.set(tx);
}

pub fn send(cmd: Cmd) {
    if let Some(tx) = CMD.get() {
        let _ = tx.send(cmd);
    }
}

/// Set when something changed; the UI timer redraws and clears it.
pub static DIRTY: AtomicBool = AtomicBool::new(true);

pub fn changed() {
    DIRTY.store(true, Ordering::Release);
}

pub fn set_status(s: impl Into<String>) {
    state().status = s.into();
    changed();
}
