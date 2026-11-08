//! The Go Live window: pick a screen, resolution, frame rate and whether to
//! share PC sound, then go live. It only exists while it's open.

use crate::golive;
use crate::state::{self, state, Cmd, LiveCfg};
use crate::store;
use crate::ui::{child, combo_add, main_info, sc, set_text, wide};
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::SS_LEFT;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_MON: i32 = 201;
const ID_RES: i32 = 202;
const ID_FPS: i32 = 203;
const ID_SOUND: i32 = 204;
const ID_GO: i32 = 205;
const ID_NOTE: i32 = 206;
const ID_STATUS: i32 = 207;

struct LiveUi {
    hwnd: HWND,
    mon: HWND,
    res: HWND,
    fps: HWND,
    sound: HWND,
    go: HWND,
    note: HWND,
    status: HWND,
    monitors: Vec<crate::capture::Monitor>,
    res_items: Vec<u32>,
    fps_items: Vec<u32>,
    allowed: (u32, u32),
    shown: String,
}

thread_local! {
    static LIVE: RefCell<Option<LiveUi>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut LiveUi) -> R) -> Option<R> {
    LIVE.with(|l| l.borrow_mut().as_mut().map(f))
}

pub fn hwnd() -> HWND {
    with(|l| l.hwnd).unwrap_or(null_mut())
}

pub unsafe fn register(hinst: HINSTANCE) {
    let cls = wide("tinycord_live");
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

/// What's allowed in the channel you're in (or the server you're looking at).
fn allowed_now() -> (u32, u32) {
    let s = state();
    let g = s.joined.map(|(g, _)| g).unwrap_or(s.selected_guild);
    let boost = s.guilds.iter().find(|x| x.id == g).map(|x| x.boost).unwrap_or(0);
    golive::allowed(s.premium, boost)
}

pub unsafe fn open() {
    let h = hwnd();
    if !h.is_null() {
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
        return;
    }
    let Some((main, _, dpi)) = main_info() else { return };
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let mut r = RECT { left: 0, top: 0, right: sc(dpi, 280), bottom: sc(dpi, 214) };
    AdjustWindowRect(&mut r, style, 0);
    let mut mr: RECT = std::mem::zeroed();
    GetWindowRect(main, &mut mr);
    let cls = wide("tinycord_live");
    let title = wide("Go Live");
    let h = CreateWindowExW(
        0,
        cls.as_ptr(),
        title.as_ptr(),
        style,
        mr.right + sc(dpi, 8),
        mr.top,
        r.right - r.left,
        r.bottom - r.top,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if !h.is_null() {
        ShowWindow(h, SW_SHOW);
    }
}

unsafe fn build(hwnd: HWND) {
    let Some((_, font, dpi)) = main_info() else { return };
    let ts = WS_TABSTOP;
    let label = |t: &str, x, y, w| child(hwnd, "STATIC", t, SS_LEFT as u32, x, y, w, 18, 0, dpi, font);
    let cbo = |x, y, w, id| {
        child(hwnd, "COMBOBOX", "", ts | WS_VSCROLL | CBS_DROPDOWNLIST as u32, x, y, w, 200, id, dpi, font)
    };
    label("Screen", 8, 11, 70);
    let mon = cbo(80, 8, 192, ID_MON);
    label("Resolution", 8, 41, 70);
    let res = cbo(80, 38, 92, ID_RES);
    label("FPS", 182, 41, 30);
    let fps = cbo(212, 38, 60, ID_FPS);
    let sound = child(hwnd, "BUTTON", "Share PC sound", ts | BS_AUTOCHECKBOX as u32, 10, 68, 200, 20, ID_SOUND, dpi, font);
    let note = child(hwnd, "STATIC", "", SS_LEFT as u32, 8, 92, 264, 48, ID_NOTE, dpi, font);
    let go = child(hwnd, "BUTTON", "Go Live", ts | BS_DEFPUSHBUTTON as u32, 8, 144, 264, 30, ID_GO, dpi, font);
    let status = child(hwnd, "STATIC", "", SS_LEFT as u32, 8, 180, 264, 32, ID_STATUS, dpi, font);

    let monitors = crate::capture::monitors();
    let names: Vec<String> = if monitors.is_empty() {
        vec!["Main screen".to_string()]
    } else {
        monitors.iter().map(|m| format!("{}  {}x{}", m.name, m.w, m.h)).collect()
    };
    combo_add(mon, &names.iter().map(String::as_str).collect::<Vec<_>>());

    LIVE.with(|l| {
        *l.borrow_mut() = Some(LiveUi {
            hwnd,
            mon,
            res,
            fps,
            sound,
            go,
            note,
            status,
            monitors,
            res_items: Vec::new(),
            fps_items: Vec::new(),
            allowed: (u32::MAX, 0),
            shown: String::new(),
        })
    });
    let cfg = state().live_cfg;
    let m = with(|l| crate::capture::resolve(cfg.monitor, &l.monitors)).unwrap_or(0).min(names.len() - 1);
    SendMessageW(mon, CB_SETCURSEL, m, 0);
    SendMessageW(sound, BM_SETCHECK, cfg.sound as usize, 0);
    fill_options();
    update();
}

/// Resolution and FPS lists: only what your account / the server allows and
/// what the chosen screen can show.
unsafe fn fill_options() {
    let allowed = allowed_now();
    let Some((res, fps, mon_h, mon_w)) = with(|l| {
        let i = SendMessageW(l.mon, CB_GETCURSEL, 0, 0).max(0) as usize;
        let (w, h) = l.monitors.get(i).map(|m| (m.w, m.h)).unwrap_or((1920, 1080));
        l.allowed = allowed;
        (l.res, l.fps, h, w)
    }) else {
        return;
    };
    let (max_h, max_fps) = allowed;
    let cfg = state().live_cfg;
    let mut res_items = Vec::new();
    let mut labels = Vec::new();
    for &h in &golive::HEIGHTS {
        if !golive::height_ok(h, max_h) {
            continue;
        }
        if h != 0 && h > mon_h {
            continue; // never upscale
        }
        res_items.push(h);
        labels.push(if h == 0 { format!("Source ({mon_w}x{mon_h})") } else { format!("{h}p") });
    }
    if res_items.is_empty() {
        res_items.push(max_h.min(mon_h));
        labels.push(format!("{}p", max_h.min(mon_h)));
    }
    let fps_items: Vec<u32> = golive::FPS.iter().copied().filter(|f| *f <= max_fps).collect();
    SendMessageW(res, CB_RESETCONTENT, 0, 0);
    combo_add(res, &labels.iter().map(String::as_str).collect::<Vec<_>>());
    SendMessageW(fps, CB_RESETCONTENT, 0, 0);
    let fl: Vec<String> = fps_items.iter().map(|f| f.to_string()).collect();
    combo_add(fps, &fl.iter().map(String::as_str).collect::<Vec<_>>());
    // Keep the saved choice if it's still offered, else the best allowed.
    let ri = res_items
        .iter()
        .position(|h| *h == cfg.height)
        .unwrap_or_else(|| res_items.iter().position(|h| *h == 720).unwrap_or(res_items.len() - 1));
    let fi = fps_items.iter().position(|f| *f == cfg.fps).unwrap_or(fps_items.len().saturating_sub(1));
    SendMessageW(res, CB_SETCURSEL, ri, 0);
    SendMessageW(fps, CB_SETCURSEL, fi, 0);
    let note = format!(
        "Allowed here: up to {} at {} fps (your Nitro or this server's boost level).\nEncoding uses your CPU; 1080p 60 needs a fast one.",
        if max_h == 0 { "any size".to_string() } else { format!("{max_h}p") },
        max_fps
    );
    with(|l| {
        set_text(l.note, &note);
        l.res_items = res_items;
        l.fps_items = fps_items;
    });
}

/// Settings as shown in the window.
unsafe fn read_cfg() -> Option<LiveCfg> {
    with(|l| {
        let ri = SendMessageW(l.res, CB_GETCURSEL, 0, 0).max(0) as usize;
        let fi = SendMessageW(l.fps, CB_GETCURSEL, 0, 0).max(0) as usize;
        LiveCfg {
            monitor: SendMessageW(l.mon, CB_GETCURSEL, 0, 0).max(0) as usize,
            height: l.res_items.get(ri).copied().unwrap_or(720),
            fps: l.fps_items.get(fi).copied().unwrap_or(30),
            sound: SendMessageW(l.sound, BM_GETCHECK, 0, 0) == 1,
        }
    })
}

unsafe fn settings_changed() {
    let Some(cfg) = read_cfg() else { return };
    let live = {
        let mut s = state();
        s.live_cfg = cfg;
        s.live.is_some()
    };
    store::save_settings();
    if live {
        state::send(Cmd::GoLive(cfg)); // applies the new settings right away
    }
}

/// Refresh the button and status line (called by the main window's timer).
pub unsafe fn update() {
    if hwnd().is_null() {
        return;
    }
    // Moving to another server can change what's allowed.
    if with(|l| l.allowed).is_some_and(|a| a != allowed_now()) {
        fill_options();
    }
    let (live, joined, ready) = {
        let s = state();
        (s.live.clone(), s.joined.is_some(), s.voice_ready)
    };
    let text = match &live {
        Some(l) => {
            let mut t = format!("LIVE - {} watching", l.viewers);
            if l.w > 0 {
                t += &format!("\n{}x{}  {} fps  {:.1} Mbit/s", l.w, l.h, l.fps, l.kbps as f64 / 1000.0);
            }
            if !l.status.is_empty() && l.status != "Live" {
                t = format!("{}\n{}", l.status, t);
            }
            if !l.sound_note.is_empty() {
                t += &format!("\n{}", l.sound_note);
            }
            t
        }
        None if !joined => "Join a voice channel first.".into(),
        None if !ready => "Connecting to voice...".into(),
        None => "Not live.".into(),
    };
    with(|l| {
        if l.shown != text {
            set_text(l.status, &text);
            set_text(l.go, if live.is_some() { "Stop" } else { "Go Live" });
            l.shown = text;
        }
    });
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            build(hwnd);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            match (id, code) {
                (ID_MON, CBN_SELCHANGE) => {
                    fill_options();
                    settings_changed();
                }
                (ID_RES | ID_FPS, CBN_SELCHANGE) | (ID_SOUND, BN_CLICKED) => settings_changed(),
                (ID_GO, BN_CLICKED) => {
                    if state().live.is_some() {
                        state::send(Cmd::StopLive);
                    } else if let Some(cfg) = read_cfg() {
                        state().live_cfg = cfg;
                        store::save_settings();
                        state::send(Cmd::GoLive(cfg));
                    }
                }
                _ => {}
            }
            let _ = (ID_NOTE, ID_STATUS);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            LIVE.with(|l| *l.borrow_mut() = None);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
