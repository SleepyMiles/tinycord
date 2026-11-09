//! The chat window (exists only while open) and ping notifications.
//!
//! Pings show as a normal Windows notification (from a small tray icon
//! that appears with the first one); clicking it opens that chat.

use crate::chat;
use crate::state::{self, state, Cmd};
use crate::store;
use crate::ui::{child, combo_add, main_info, sc, set_text, wide};
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{EM_GETFIRSTVISIBLELINE, EM_LINESCROLL, EM_SETLIMITTEXT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_SRV: i32 = 301;
const ID_CHAN: i32 = 302;
const ID_MSGS: i32 = 303;
const ID_INPUT: i32 = 304;
const ID_STATUS: i32 = 305;
/// Send is the window's OK button, so Enter in the text box sends.
const ID_SEND: i32 = IDOK;
/// Messages from the tray icon to the main window.
pub const WM_TRAY: u32 = WM_APP + 7;

struct ChatUi {
    hwnd: HWND,
    srv: HWND,
    chan: HWND,
    msgs: HWND,
    input: HWND,
    send: HWND,
    status: HWND,
    dpi: i32,
    /// Server list entries (0 = direct messages) and channel list entries.
    guild_ids: Vec<u64>,
    chan_ids: Vec<u64>,
    shown: Option<(u64, u64)>, // (channel, version) on screen
}

thread_local! {
    static CHAT: RefCell<Option<ChatUi>> = const { RefCell::new(None) };
    static TRAY: Cell<bool> = const { Cell::new(false) };
    static SHOWN_PING: Cell<u64> = const { Cell::new(0) };
}

fn with<R>(f: impl FnOnce(&mut ChatUi) -> R) -> Option<R> {
    CHAT.with(|c| c.borrow_mut().as_mut().map(f))
}

pub fn hwnd() -> HWND {
    with(|c| c.hwnd).unwrap_or(null_mut())
}

pub unsafe fn register(hinst: HINSTANCE) {
    let cls = wide("tinycord_chat");
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(proc_),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: cls.as_ptr(),
    };
    RegisterClassW(&wc);
}

/// Open the chat window (or bring it forward), optionally at a channel.
pub unsafe fn open(target: Option<(u64, u64)>) {
    let h = hwnd();
    if h.is_null() {
        let Some((main, _, dpi)) = main_info() else { return };
        let mut mr: RECT = std::mem::zeroed();
        GetWindowRect(main, &mut mr);
        let cls = wide("tinycord_chat");
        let title = wide("tinycord chat");
        let h = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            mr.right + sc(dpi, 8),
            mr.top,
            sc(dpi, 480),
            sc(dpi, 460),
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if h.is_null() {
            return;
        }
        ShowWindow(h, SW_SHOW);
    } else {
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
    }
    // Where to start: the ping that was clicked, the chat open last time,
    // the server of your voice channel, or your DMs.
    let pick = target.or_else(|| {
        let s = state();
        let last = s.chat_last;
        let known = last.1 != 0
            && (s.dms.iter().any(|d| d.id == last.1)
                || s.guilds.iter().any(|g| g.id == last.0 && g.text.iter().any(|c| c.id == last.1)));
        if known {
            return Some(last);
        }
        let g = s.joined.map(|(g, _)| g).unwrap_or(s.selected_guild);
        s.guilds.iter().find(|x| x.id == g).and_then(|x| x.text.first()).map(|c| (g, c.id))
    });
    let open_now = state().chat.as_ref().map(|c| (c.guild, c.channel));
    match pick {
        Some(p) if open_now != Some(p) => select(p.0, Some(p.1)),
        None if open_now.is_none() => select(0, None),
        _ => {}
    }
    if let Some(i) = with(|c| c.input) {
        SetFocus(i);
    }
}

unsafe fn build(hwnd: HWND) {
    let Some((_, font, dpi)) = main_info() else { return };
    let ts = WS_TABSTOP;
    let cbo = |id| child(hwnd, "COMBOBOX", "", ts | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 0, 0, 10, 300, id, dpi, font);
    let srv = cbo(ID_SRV);
    let chan = cbo(ID_CHAN);
    let msgs = child(
        hwnd,
        "EDIT",
        "",
        WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32 | WS_BORDER,
        0,
        0,
        10,
        10,
        ID_MSGS,
        dpi,
        font,
    );
    SendMessageW(msgs, EM_SETLIMITTEXT, 1 << 22, 0);
    let status = child(hwnd, "STATIC", "", 0, 0, 0, 10, 10, ID_STATUS, dpi, font);
    let input = child(hwnd, "EDIT", "", ts | ES_AUTOHSCROLL as u32 | WS_BORDER, 0, 0, 10, 10, ID_INPUT, dpi, font);
    SendMessageW(input, EM_SETLIMITTEXT, 2000, 0); // Discord's limit
    let send = child(hwnd, "BUTTON", "Send", ts | BS_DEFPUSHBUTTON as u32, 0, 0, 10, 10, ID_SEND, dpi, font);
    CHAT.with(|c| {
        *c.borrow_mut() = Some(ChatUi {
            hwnd,
            srv,
            chan,
            msgs,
            input,
            send,
            status,
            dpi,
            guild_ids: Vec::new(),
            chan_ids: Vec::new(),
            shown: None,
        })
    });
    fill_servers();
    let mut r: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut r);
    layout(r.right, r.bottom);
}

unsafe fn layout(w: i32, h: i32) {
    with(|c| {
        let s = |v| sc(c.dpi, v);
        let m = s(6);
        let half = (w - 3 * m) * 2 / 5;
        MoveWindow(c.srv, m, m, half, s(300), 1);
        MoveWindow(c.chan, 2 * m + half, m, w - 3 * m - half, s(300), 1);
        let top = m + s(28);
        let bottom = h - m - s(26) - s(22);
        MoveWindow(c.msgs, m, top, w - 2 * m, (bottom - top).max(s(40)), 1);
        MoveWindow(c.status, m, bottom + s(3), w - 2 * m, s(18), 1);
        MoveWindow(c.input, m, h - m - s(26), w - 3 * m - s(64), s(26), 1);
        MoveWindow(c.send, w - m - s(64), h - m - s(26), s(64), s(26), 1);
    });
}

/// " (@2)" when there are pings you haven't looked at.
fn marks(n: usize) -> String {
    if n > 0 {
        format!("  (@{n})")
    } else {
        String::new()
    }
}

unsafe fn fill_servers() {
    let (names, ids): (Vec<String>, Vec<u64>) = {
        let s = state();
        let count = |g: u64| s.pings.iter().filter(|p| p.guild == g).count();
        std::iter::once((format!("Direct messages{}", marks(count(0))), 0))
            .chain(s.guilds.iter().map(|g| (format!("{}{}", g.name, marks(count(g.id))), g.id)))
            .unzip()
    };
    let Some(srv) = with(|c| {
        c.guild_ids = ids;
        c.srv
    }) else {
        return;
    };
    let keep = SendMessageW(srv, CB_GETCURSEL, 0, 0);
    SendMessageW(srv, CB_RESETCONTENT, 0, 0);
    combo_add(srv, &names.iter().map(String::as_str).collect::<Vec<_>>());
    if keep >= 0 {
        SendMessageW(srv, CB_SETCURSEL, keep as usize, 0);
    }
}

unsafe fn fill_channels(guild: u64) {
    let (names, ids): (Vec<String>, Vec<u64>) = {
        let s = state();
        let count = |c: u64| s.pings.iter().filter(|p| p.channel == c).count();
        if guild == 0 {
            s.dms.iter().map(|d| (format!("{}{}", d.name, marks(count(d.id))), d.id)).unzip()
        } else {
            s.guilds
                .iter()
                .find(|g| g.id == guild)
                .map(|g| {
                    g.text
                        .iter()
                        .map(|c| {
                            let mark = match c.kind {
                                2 => "\u{1F50A} ",
                                5 => "\u{1F4E2} ",
                                _ => "# ",
                            };
                            (format!("{mark}{}{}", c.name, marks(count(c.id))), c.id)
                        })
                        .unzip()
                })
                .unwrap_or_default()
        }
    };
    let Some(chan) = with(|c| {
        c.chan_ids = ids;
        c.chan
    }) else {
        return;
    };
    SendMessageW(chan, CB_RESETCONTENT, 0, 0);
    combo_add(chan, &names.iter().map(String::as_str).collect::<Vec<_>>());
}

/// Show a server (0 = DMs) and open a channel in it (or its first one).
unsafe fn select(guild: u64, channel: Option<u64>) {
    let Some((srv, gi)) = with(|c| (c.srv, c.guild_ids.iter().position(|g| *g == guild))) else { return };
    let gi = gi.unwrap_or(0);
    SendMessageW(srv, CB_SETCURSEL, gi, 0);
    let guild = with(|c| c.guild_ids.get(gi).copied().unwrap_or(0)).unwrap_or(0);
    fill_channels(guild);
    let Some((chan, ci, cid)) = with(|c| {
        let ci = channel.and_then(|ch| c.chan_ids.iter().position(|x| *x == ch)).unwrap_or(0);
        (c.chan, ci, c.chan_ids.get(ci).copied())
    }) else {
        return;
    };
    match cid {
        Some(cid) => {
            SendMessageW(chan, CB_SETCURSEL, ci, 0);
            open_channel(guild, cid);
        }
        None => {
            let empty = if guild == 0 { "No direct messages" } else { "No text channels here" };
            if let Some(st) = with(|c| c.status) {
                set_text(st, empty);
            }
        }
    }
}

unsafe fn open_channel(guild: u64, channel: u64) {
    state().chat_last = (guild, channel);
    store::save_settings();
    state::send(Cmd::ChatOpen(guild, channel));
}

unsafe fn send_input() {
    let Some((input, open)) = with(|c| (c.input, state().chat.as_ref().map(|x| x.channel))) else { return };
    let Some(channel) = open else { return };
    let len = GetWindowTextLengthW(input);
    if len <= 0 {
        return;
    }
    let mut buf = vec![0u16; len as usize + 1];
    GetWindowTextW(input, buf.as_mut_ptr(), buf.len() as i32);
    let text = String::from_utf16_lossy(&buf[..len as usize]);
    set_text(input, "");
    state::send(Cmd::ChatSend(channel, text));
}

/// Called by the main window whenever something changed: redraw the open
/// chat and show notifications for new pings.
pub unsafe fn update() {
    notify_pings();
    let Some((msgs, status, shown)) = with(|c| (c.msgs, c.status, c.shown)) else { return };
    let (key, text, note) = {
        let s = state();
        let Some(c) = s.chat.as_ref() else { return };
        let key = (c.channel, c.version);
        if shown == Some(key) {
            return;
        }
        let mut t = String::new();
        for m in &c.msgs {
            let body = m.text.replace("\r\n", "\n").replace('\n', "\r\n      ");
            t += &format!("[{}] {}: {}\r\n", chat::time_of(m.id), m.author, body);
        }
        (key, t, c.status.clone())
    };
    with(|c| c.shown = Some(key));
    // Stay where you are if you scrolled up to read; else follow new messages.
    let mut si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_ALL,
        nMin: 0,
        nMax: 0,
        nPage: 0,
        nPos: 0,
        nTrackPos: 0,
    };
    let following = GetScrollInfo(msgs, SB_VERT as i32, &mut si) == 0
        || si.nPos + si.nPage as i32 >= si.nMax
        || shown.map(|s| s.0) != Some(key.0);
    let first = SendMessageW(msgs, EM_GETFIRSTVISIBLELINE, 0, 0);
    set_text(msgs, &text);
    if following {
        SendMessageW(msgs, WM_VSCROLL, SB_BOTTOM as usize, 0);
    } else {
        SendMessageW(msgs, EM_LINESCROLL, 0, first);
    }
    set_text(status, &note);
}

/// Copy text into a fixed-size UTF-16 field (cut to fit).
fn put(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for u in s.encode_utf16() {
        if n + 1 >= dst.len() {
            break;
        }
        dst[n] = u;
        n += 1;
    }
    dst[n] = 0;
}

unsafe fn tray_data() -> Option<NOTIFYICONDATAW> {
    let (main, _, _) = main_info()?;
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = main;
    nid.uID = 1;
    Some(nid)
}

unsafe fn notify_pings() {
    let (seq, last) = {
        let s = state();
        (s.ping_seq, s.pings.last().cloned())
    };
    if seq <= SHOWN_PING.get() {
        return;
    }
    SHOWN_PING.set(seq);
    if !hwnd().is_null() {
        fill_servers(); // show where the new ping is
    }
    let Some(p) = last else { return };
    let Some(mut nid) = tray_data() else { return };
    if !TRAY.get() {
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        put(&mut nid.szTip, "tinycord");
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            return;
        }
        TRAY.set(true);
    }
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    put(&mut nid.szInfoTitle, &p.title);
    put(&mut nid.szInfo, if p.text.is_empty() { "(no text)" } else { &p.text });
    Shell_NotifyIconW(NIM_MODIFY, &nid);
    // Also blink the taskbar button if tinycord isn't in front.
    if let Some((main, _, _)) = main_info() {
        let fi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: main,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 3,
            dwTimeout: 0,
        };
        FlashWindowEx(&fi);
    }
}

/// The tray icon or its notification was clicked.
pub unsafe fn on_tray(lp: LPARAM) {
    match lp as u32 {
        NIN_BALLOONUSERCLICK => {
            let target = state().pings.last().map(|p| (p.guild, p.channel));
            open(target);
        }
        WM_LBUTTONUP => {
            if let Some((main, _, _)) = main_info() {
                ShowWindow(main, SW_RESTORE);
                SetForegroundWindow(main);
            }
        }
        _ => {}
    }
}

pub unsafe fn remove_tray() {
    if TRAY.get() {
        if let Some(nid) = tray_data() {
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        TRAY.set(false);
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            build(hwnd);
            0
        }
        WM_SIZE => {
            layout((lp & 0xFFFF) as i32, ((lp >> 16) & 0xFFFF) as i32);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            match (id, code) {
                (ID_SRV, CBN_SELCHANGE) => {
                    let g = with(|c| {
                        let i = SendMessageW(c.srv, CB_GETCURSEL, 0, 0).max(0) as usize;
                        c.guild_ids.get(i).copied().unwrap_or(0)
                    });
                    if let Some(g) = g {
                        select(g, None);
                    }
                }
                (ID_CHAN, CBN_SELCHANGE) => {
                    let pick = with(|c| {
                        let gi = SendMessageW(c.srv, CB_GETCURSEL, 0, 0).max(0) as usize;
                        let ci = SendMessageW(c.chan, CB_GETCURSEL, 0, 0).max(0) as usize;
                        (c.guild_ids.get(gi).copied().unwrap_or(0), c.chan_ids.get(ci).copied())
                    });
                    if let Some((g, Some(ch))) = pick {
                        open_channel(g, ch);
                    }
                }
                (ID_SEND, _) => send_input(),
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            CHAT.with(|c| *c.borrow_mut() = None);
            state::send(Cmd::ChatClose);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
