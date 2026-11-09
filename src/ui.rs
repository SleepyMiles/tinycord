//! Plain Win32 UI: one small window (servers, channels, who's in them) and an
//! always-on-top, click-through overlay that shows who is talking.
//! No browser engine, no GPU, no framework — that's where the RAM saving is.

#![allow(clippy::missing_safety_doc)]

use crate::state::{self, state, Cmd, MicMode};
use crate::store;
use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{SS_ENDELLIPSIS, SS_LEFT};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_STATUS: i32 = 100;
const ID_TOKEN: i32 = 101;
const ID_LOGIN: i32 = 102;
const ID_GUILD: i32 = 103;
const ID_LIST: i32 = 104;
const ID_JOIN: i32 = 105;
const ID_MUTE: i32 = 106;
const ID_DEAF: i32 = 107;
const ID_MODE: i32 = 108;
const ID_KEY: i32 = 109;
const ID_SENS: i32 = 110;
const ID_OVERLAY: i32 = 111;
const ID_SPK: i32 = 112;
const ID_TOP: i32 = 113;
const ID_NS: i32 = 114;
const ID_AGC: i32 = 115;
const ID_MIC: i32 = 116;
const ID_SPKDEV: i32 = 117;
const ID_VOL: i32 = 118;
const ID_NSLVL: i32 = 119;
const ID_LIVE: i32 = 120;
const ID_LOGOUT: i32 = 121;
const ID_CHAT: i32 = 122;

const TIMER_REFRESH: usize = 1;
const TIMER_TOPMOST: usize = 2;

const OVERLAY_NAMES: [&str; 6] = [
    "Overlay: off",
    "Overlay: top-left",
    "Overlay: top-right",
    "Overlay: bottom-left",
    "Overlay: bottom-right",
    "Overlay: left-middle",
];

#[derive(Clone, PartialEq)]
enum RowKind {
    Chan,
    Member,
}

#[derive(Clone, PartialEq)]
struct Row {
    kind: RowKind,
    id: u64,
    channel: u64,
    text: String,
    talking: bool,
    dim: bool,
    here: bool,
    live: bool,
}

struct Ui {
    hwnd: HWND,
    overlay: HWND,
    c: [HWND; 23],
    font: HFONT,
    bold: HFONT,
    ofont: HFONT,
    dpi: i32,
    rows: Vec<Row>,
    guild_cache: Vec<(u64, String)>,
    status_cache: String,
    overlay_lines: Vec<(String, bool, bool)>,
    logged_in_shown: Option<bool>,
    keep_on_top: bool,
    dev_cache: (Vec<String>, Vec<String>),
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// (main window, normal font, dpi) for the other windows.
pub(crate) fn main_info() -> Option<(HWND, HFONT, i32)> {
    with_ui(|u| (u.hwnd, u.font, u.dpi))
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.borrow_mut().as_mut().map(f))
}

pub(crate) fn sc(dpi: i32, v: i32) -> i32 {
    v * dpi / 96
}

pub fn key_down(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

fn key_name(vk: u32) -> String {
    match vk {
        0 => return "not set".into(),
        4 => return "Mouse 3".into(),
        5 => return "Mouse 4".into(),
        6 => return "Mouse 5".into(),
        _ => {}
    }
    unsafe {
        let mut scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) as i32;
        // Extended keys (arrows, Insert/Delete, right Ctrl/Alt...) need bit 24.
        if matches!(vk, 0x21..=0x2E | 0xA3 | 0xA5 | 0x5B | 0x5C | 0x6F | 0x90) {
            scan |= 0x100;
        }
        let mut buf = [0u16; 64];
        let n = GetKeyNameTextW(scan << 16, buf.as_mut_ptr(), 64);
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("Key {vk}")
        }
    }
}

pub(crate) unsafe fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    SetWindowTextW(h, w.as_ptr());
}

unsafe fn get_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h);
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, buf.as_mut_ptr(), n + 1);
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

pub(crate) unsafe fn make_font(dpi: i32, pt: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    CreateFontW(
        -(pt * dpi / 72),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        (DEFAULT_PITCH | FF_DONTCARE) as u32,
        face.as_ptr(),
    )
}

pub fn run() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let dpi = GetDpiForSystem() as i32;
        let hinst = GetModuleHandleW(null());

        let main_cls = wide("tinycord_main");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(main_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: main_cls.as_ptr(),
        };
        RegisterClassW(&wc);

        let ov_cls = wide("tinycord_overlay");
        let wc2 = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(overlay_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: ov_cls.as_ptr(),
        };
        RegisterClassW(&wc2);

        let sl_cls = wide("tinycord_slider");
        let wc3 = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(slider_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_HAND),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: sl_cls.as_ptr(),
        };
        RegisterClassW(&wc3);

        let view_cls = wide("tinycord_view");
        let wc4 = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(view_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: view_cls.as_ptr(),
        };
        RegisterClassW(&wc4);
        if let Ok(mut f) = crate::video::ON_FRAME.write() {
            *f = Some(view_repaint);
        }
        crate::ui_live::register(hinst);
        crate::ui_chat::register(hinst);

        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                hwnd: null_mut(),
                overlay: null_mut(),
                c: [null_mut(); 23],
                font: make_font(dpi, 9, FW_NORMAL as i32),
                bold: make_font(dpi, 9, FW_BOLD as i32),
                ofont: make_font(dpi, 11, FW_SEMIBOLD as i32),
                dpi,
                rows: Vec::new(),
                guild_cache: Vec::new(),
                status_cache: String::new(),
                overlay_lines: Vec::new(),
                logged_in_shown: None,
                keep_on_top: false,
                dev_cache: (Vec::new(), Vec::new()),
            })
        });

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: sc(dpi, 280), bottom: sc(dpi, 510) };
        AdjustWindowRect(&mut r, style, 0);
        let title = wide("tinycord");
        let hwnd = CreateWindowExW(
            0,
            main_cls.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );

        let overlay = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            ov_cls.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        SetLayeredWindowAttributes(overlay, 0, 210, LWA_ALPHA);
        with_ui(|u| u.overlay = overlay);

        if state().keep_on_top {
            with_ui(|u| u.keep_on_top = true);
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
        ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, TIMER_REFRESH, 50, None);
        SetTimer(hwnd, TIMER_TOPMOST, 2000, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let lw = crate::ui_live::hwnd();
            if !lw.is_null() && IsDialogMessageW(lw, &msg) != 0 {
                continue;
            }
            let cw = crate::ui_chat::hwnd();
            if !cw.is_null() && IsDialogMessageW(cw, &msg) != 0 {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

pub(crate) unsafe fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    dpi: i32,
    font: HFONT,
) -> HWND {
    let cls = wide(class);
    let t = wide(text);
    let ex = if class == "EDIT" || class == "LISTBOX" { WS_EX_CLIENTEDGE } else { 0 };
    let h = CreateWindowExW(
        ex,
        cls.as_ptr(),
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        sc(dpi, x),
        sc(dpi, y),
        sc(dpi, w),
        sc(dpi, h),
        parent,
        id as usize as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
    SendMessageW(h, WM_SETFONT, font as usize, 1);
    h
}

pub(crate) unsafe fn combo_add(h: HWND, items: &[&str]) {
    for it in items {
        let w = wide(it);
        SendMessageW(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
    }
}

unsafe fn create_controls(hwnd: HWND) {
    let (dpi, font) = with_ui(|u| (u.dpi, u.font)).unwrap();
    let ts = WS_TABSTOP;
    let b = |text: &str, style: i32, x, y, w, h, id| {
        child(hwnd, "BUTTON", text, ts | style as u32, x, y, w, h, id, dpi, font)
    };
    let cbo = |x, y, w, id| {
        child(hwnd, "COMBOBOX", "", ts | WS_VSCROLL | CBS_DROPDOWNLIST as u32, x, y, w, 300, id, dpi, font)
    };
    let mut c = [null_mut(); 23];
    c[0] = child(hwnd, "STATIC", "Starting...", SS_LEFT as u32 | SS_ENDELLIPSIS as u32, 6, 6, 268, 18, ID_STATUS, dpi, font);
    c[1] = child(hwnd, "EDIT", "", ts | ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32, 6, 28, 190, 24, ID_TOKEN, dpi, font);
    c[2] = b("Log in", BS_PUSHBUTTON, 202, 28, 72, 24, ID_LOGIN);
    c[3] = cbo(6, 28, 190, ID_GUILD);
    c[21] = b("Log out", BS_PUSHBUTTON, 202, 28, 72, 24, ID_LOGOUT);
    c[4] = child(
        hwnd,
        "LISTBOX",
        "",
        ts | WS_VSCROLL | (LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT) as u32,
        6,
        58,
        268,
        200,
        ID_LIST,
        dpi,
        font,
    );
    c[5] = b("Join", BS_PUSHBUTTON, 6, 264, 64, 26, ID_JOIN);
    c[6] = b("Mute", BS_AUTOCHECKBOX | BS_PUSHLIKE, 74, 264, 64, 26, ID_MUTE);
    c[7] = b("Deafen", BS_AUTOCHECKBOX | BS_PUSHLIKE, 142, 264, 64, 26, ID_DEAF);
    c[20] = b("Go Live", BS_PUSHBUTTON, 210, 264, 64, 26, ID_LIVE);
    c[8] = cbo(6, 296, 130, ID_MODE);
    c[9] = b("PTT key: not set", BS_PUSHBUTTON, 142, 295, 132, 25, ID_KEY);
    c[10] = child(hwnd, "tinycord_slider", "", 0, 6, 326, 268, 22, ID_SENS, dpi, font);
    c[18] = child(hwnd, "tinycord_slider", "", 0, 6, 352, 268, 22, ID_VOL, dpi, font);
    c[19] = child(hwnd, "tinycord_slider", "", 0, 6, 378, 268, 22, ID_NSLVL, dpi, font);
    c[11] = cbo(6, 406, 130, ID_OVERLAY);
    c[12] = b("Speakers only", BS_AUTOCHECKBOX, 144, 406, 128, 22, ID_SPK);
    c[14] = b("Clarity boost", BS_AUTOCHECKBOX, 8, 432, 128, 22, ID_NS);
    c[15] = b("Auto gain", BS_AUTOCHECKBOX, 144, 432, 128, 22, ID_AGC);
    c[13] = b("Keep on top", BS_AUTOCHECKBOX, 8, 456, 128, 22, ID_TOP);
    c[22] = b("Chat", BS_PUSHBUTTON, 142, 454, 132, 24, ID_CHAT);
    c[16] = cbo(6, 480, 130, ID_MIC);
    c[17] = cbo(142, 480, 132, ID_SPKDEV);
    SendMessageW(c[16], CB_SETDROPPEDWIDTH, sc(dpi, 320) as usize, 0);
    SendMessageW(c[17], CB_SETDROPPEDWIDTH, sc(dpi, 320) as usize, 0);

    combo_add(c[8], &["Voice activation", "Push-to-talk", "Open mic"]);
    combo_add(c[11], &OVERLAY_NAMES);

    let s = state();
    let mode_idx = match s.mode {
        MicMode::Voice => 0,
        MicMode::Ptt => 1,
        MicMode::Open => 2,
    };
    SendMessageW(c[8], CB_SETCURSEL, mode_idx, 0);
    SendMessageW(c[11], CB_SETCURSEL, s.overlay as usize, 0);
    SendMessageW(c[12], BM_SETCHECK, s.overlay_speakers_only as usize, 0);
    SendMessageW(c[13], BM_SETCHECK, s.keep_on_top as usize, 0);
    drop(s);
    let sh = crate::voice::shared();
    SendMessageW(c[14], BM_SETCHECK, sh.clarity.load(Ordering::Relaxed) as usize, 0);
    SendMessageW(c[15], BM_SETCHECK, sh.auto_gain.load(Ordering::Relaxed) as usize, 0);

    with_ui(|u| {
        u.hwnd = hwnd;
        u.c = c;
    });
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_controls(hwnd);
            0
        }
        WM_TIMER => {
            if wp == TIMER_REFRESH {
                capture_key();
                mic_meter(hwnd);
                let live = { let s = state(); s.joined.is_some() && s.voice_ready };
                if let Some(sl) = with_ui(|u| u.c[10]) {
                    if live {
                        InvalidateRect(sl, null(), 0);
                    }
                }
                if state::DIRTY.swap(false, Ordering::AcqRel) {
                    refresh();
                }
            } else if wp == TIMER_TOPMOST {
                if let Some(ov) = with_ui(|u| u.overlay) {
                    if IsWindowVisible(ov) != 0 {
                        SetWindowPos(ov, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = ((wp >> 16) & 0xFFFF) as u32;
            on_command(id, code);
            0
        }
        WM_MEASUREITEM => {
            let mi = lp as *mut MEASUREITEMSTRUCT;
            let dpi = with_ui(|u| u.dpi).unwrap_or(96);
            (*mi).itemHeight = sc(dpi, 20) as u32;
            1
        }
        WM_DRAWITEM => {
            draw_item(&*(lp as *const DRAWITEMSTRUCT));
            1
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        crate::ui_chat::WM_TRAY => {
            crate::ui_chat::on_tray(lp);
            0
        }
        WM_DESTROY => {
            crate::ui_chat::remove_tray();
            store::save_settings();
            if state().joined.is_some() {
                state::send(Cmd::Leave);
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn on_command(id: i32, code: u32) {
    let Some(c) = with_ui(|u| u.c) else { return };
    match (id, code) {
        (ID_LOGIN, BN_CLICKED) => {
            let t = get_text(c[1]);
            set_text(c[1], "");
            state::set_status("Logging in...");
            state::send(Cmd::Login(t));
        }
        (ID_GUILD, CBN_SELCHANGE) => {
            let i = SendMessageW(c[3], CB_GETCURSEL, 0, 0);
            let gid = with_ui(|u| u.guild_cache.get(i as usize).map(|g| g.0)).flatten();
            if let Some(g) = gid {
                state().selected_guild = g;
                state::send(Cmd::ViewGuild(g));
                state::changed();
            }
        }
        (ID_LIST, LBN_DBLCLK) => join_selected(),
        (ID_LIST, LBN_SELCHANGE) => state::changed(),
        (ID_JOIN, BN_CLICKED) => {
            if selected_live() {
                join_selected();
            } else if state().joined.is_some() {
                state::send(Cmd::Leave);
            } else {
                join_selected();
            }
        }
        (ID_LIVE, BN_CLICKED) => {
            if state().live.is_some() {
                state::send(Cmd::StopLive);
            } else {
                crate::ui_live::open();
            }
        }
        (ID_MUTE, BN_CLICKED) => {
            state().muted = SendMessageW(c[6], BM_GETCHECK, 0, 0) == 1;
            state::send(Cmd::UpdateSelf);
        }
        (ID_DEAF, BN_CLICKED) => {
            state().deafened = SendMessageW(c[7], BM_GETCHECK, 0, 0) == 1;
            state::send(Cmd::UpdateSelf);
        }
        (ID_MODE, CBN_SELCHANGE) => {
            let i = SendMessageW(c[8], CB_GETCURSEL, 0, 0);
            state().mode = match i {
                1 => MicMode::Ptt,
                2 => MicMode::Open,
                _ => MicMode::Voice,
            };
            store::save_settings();
            state::changed();
        }
        (ID_KEY, BN_CLICKED) => {
            state().capturing_key = true;
            set_text(c[9], "Press a key... (Esc)");
        }
        (ID_OVERLAY, CBN_SELCHANGE) => {
            let i = SendMessageW(c[11], CB_GETCURSEL, 0, 0);
            state().overlay = i.clamp(0, state::OVERLAY_MAX as isize) as u8;
            store::save_settings();
            with_ui(|u| u.overlay_lines.clear());
            state::changed();
        }
        (ID_SPK, BN_CLICKED) => {
            state().overlay_speakers_only = SendMessageW(c[12], BM_GETCHECK, 0, 0) == 1;
            store::save_settings();
            state::changed();
        }
        (ID_NS, BN_CLICKED) => {
            let on = SendMessageW(c[14], BM_GETCHECK, 0, 0) == 1;
            crate::voice::shared().clarity.store(on, Ordering::Relaxed);
            store::save_settings();
        }
        (ID_AGC, BN_CLICKED) => {
            let on = SendMessageW(c[15], BM_GETCHECK, 0, 0) == 1;
            crate::voice::shared().auto_gain.store(on, Ordering::Relaxed);
            store::save_settings();
        }
        (ID_MIC | ID_SPKDEV, CBN_DROPDOWN) => crate::refresh_devices(),
        (ID_MIC, CBN_SELCHANGE) => {
            let i = SendMessageW(c[16], CB_GETCURSEL, 0, 0);
            let name = if i <= 0 { None } else { state().input_devices.get(i as usize - 1).cloned() };
            let krisp = name.as_deref().is_some_and(|n| n.to_lowercase().contains("krisp"));
            {
                let mut s = state();
                s.mic_device = name;
                s.mic_chosen = true;
            }
            if krisp {
                // Krisp already cleans the audio; doing it twice sounds worse.
                crate::voice::shared().noise_suppression.store(false, Ordering::Relaxed);
                InvalidateRect(c[19], null(), 0);
            }
            store::save_settings();
            state::send(Cmd::RestartAudio);
        }
        (ID_SPKDEV, CBN_SELCHANGE) => {
            let i = SendMessageW(c[17], CB_GETCURSEL, 0, 0);
            let name = if i <= 0 { None } else { state().output_devices.get(i as usize - 1).cloned() };
            state().spk_device = name;
            store::save_settings();
            state::send(Cmd::RestartAudio);
        }
        (ID_CHAT, BN_CLICKED) => crate::ui_chat::open(None),
        (ID_LOGOUT, BN_CLICKED) => {
            let Some(h) = with_ui(|u| u.hwnd) else { return };
            let text = wide("Log out of this Discord account?\n\nYou'll need to paste a token to log in again.");
            let title = wide("tinycord");
            if MessageBoxW(h, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONQUESTION) == IDYES {
                state::set_status("Logging out...");
                state::send(Cmd::Logout);
            }
        }
        (ID_TOP, BN_CLICKED) => {
            let on = SendMessageW(c[13], BM_GETCHECK, 0, 0) == 1;
            state().keep_on_top = on;
            store::save_settings();
            if let Some(h) = with_ui(|u| {
                u.keep_on_top = on;
                u.hwnd
            }) {
                let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
                SetWindowPos(h, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            }
        }
        _ => {}
    }
}

/// Shows the mic level in the title bar while in voice (about 4 times a second).
unsafe fn mic_meter(hwnd: HWND) {
    use std::sync::atomic::AtomicU32;
    static TICK: AtomicU32 = AtomicU32::new(0);
    if TICK.fetch_add(1, Ordering::Relaxed) % 5 != 0 {
        return;
    }
    let (in_voice, talking) = {
        let s = state();
        (s.joined.is_some() && s.voice_ready, s.self_talking)
    };
    let title = if in_voice {
        let db = f32::from_bits(crate::voice::shared().mic_level.load(Ordering::Relaxed));
        let bars = (((db + 60.0) / 6.0).clamp(0.0, 10.0)) as usize;
        let sh = crate::voice::shared();
        let gain = f32::from_bits(sh.agc_gain.load(Ordering::Relaxed));
        let clip = sh.last_clip_ms.load(Ordering::Relaxed);
        let clipping = clip != 0 && sh.now_ms().saturating_sub(clip) < 2000;
        format!(
            "tinycord  mic {}{} {:.0} dB  x{:.1}{}",
            "\u{2588}".repeat(bars),
            "\u{2591}".repeat(10 - bars),
            db.max(-99.0),
            gain,
            if talking { "  SENDING" } else { "" }
        ) + if clipping { "  MIC CLIPPING - lower Windows mic level" } else { "" }
    } else {
        "tinycord".to_string()
    };
    set_text(hwnd, &title);
}

/// Is the selected row someone else who is streaming?
unsafe fn selected_live() -> bool {
    let Some((lb, rows)) = with_ui(|u| (u.c[4], u.rows.clone())) else { return false };
    let i = SendMessageW(lb, LB_GETCURSEL, 0, 0);
    let me = state().me;
    i >= 0
        && rows
            .get(i as usize)
            .is_some_and(|r| r.kind == RowKind::Member && r.live && r.id != me)
}

unsafe fn join_selected() {
    let Some((lb, rows)) = with_ui(|u| (u.c[4], u.rows.clone())) else { return };
    let i = SendMessageW(lb, LB_GETCURSEL, 0, 0);
    if i < 0 {
        return;
    }
    let Some(row) = rows.get(i as usize) else { return };
    let g = state().selected_guild;
    if g != 0 && row.channel != 0 {
        if row.kind == RowKind::Member && row.live && row.id != state().me {
            state::send(Cmd::Watch(g, row.channel, row.id));
        } else {
            state::send(Cmd::Join(g, row.channel));
        }
    }
}

/// While "Press a key..." is active, take the first key or mouse button
/// pressed (left/right click are skipped so the click on the button itself
/// doesn't count).
unsafe fn capture_key() {
    if !state().capturing_key {
        return;
    }
    if key_down(VK_ESCAPE as u32) {
        state().capturing_key = false;
        state::changed();
        return;
    }
    for vk in 3u32..=254 {
        if matches!(vk, 0x10 | 0x11 | 0x12 | 0x1B) {
            continue; // generic Shift/Ctrl/Alt (we use the left/right ones), Esc
        }
        if key_down(vk) {
            let mut s = state();
            s.ptt_key = vk;
            s.capturing_key = false;
            if s.mode != MicMode::Ptt {
                s.mode = MicMode::Ptt;
            }
            drop(s);
            store::save_settings();
            if let Some(c) = with_ui(|u| u.c) {
                SendMessageW(c[8], CB_SETCURSEL, 1, 0);
            }
            state::changed();
            return;
        }
    }
}

fn build_rows() -> Vec<Row> {
    let s = state();
    let g = s.selected_guild;
    let Some(guild) = s.guilds.iter().find(|x| x.id == g) else { return Vec::new() };
    let mut rows = Vec::new();
    for ch in &guild.voice {
        let here = s.joined == Some((g, ch.id));
        rows.push(Row {
            kind: RowKind::Chan,
            id: ch.id,
            channel: ch.id,
            text: ch.name.clone(),
            talking: false,
            dim: false,
            here,
            live: false,
        });
        for (uid, name, vs) in s.members_of(g, ch.id) {
            let mut text = name;
            if vs.deaf {
                text.push_str("  (deaf)");
            } else if vs.mute {
                text.push_str("  (muted)");
            }
            rows.push(Row {
                kind: RowKind::Member,
                id: uid,
                channel: ch.id,
                text,
                talking: s.is_speaking(uid),
                dim: vs.mute || vs.deaf,
                here,
                live: vs.streaming,
            });
        }
    }
    rows
}

unsafe fn refresh() {
    let rows = build_rows();
    let s = state();
    let status = s.status.clone();
    let logged_in = s.logged_in;
    let guilds: Vec<(u64, String)> = s.guilds.iter().map(|g| (g.id, g.name.clone())).collect();
    let mut selected = s.selected_guild;
    let joined = s.joined.is_some();
    let (muted, deaf) = (s.muted, s.deafened);
    let key_txt = if s.capturing_key {
        "Press a key... (Esc)".to_string()
    } else {
        format!("PTT key: {}", key_name(s.ptt_key))
    };
    let mode = s.mode;
    let live = s.live.is_some();
    drop(s);

    // Pick a server automatically the first time.
    if logged_in && !guilds.iter().any(|g| g.0 == selected) {
        if let Some(first) = guilds.first() {
            selected = first.0;
            state().selected_guild = selected;
            state::send(Cmd::ViewGuild(selected));
            state::DIRTY.store(true, Ordering::Release);
        }
    }

    let Some((c, cached_guilds, cached_status, shown_login)) =
        with_ui(|u| (u.c, u.guild_cache.clone(), u.status_cache.clone(), u.logged_in_shown))
    else {
        return;
    };

    if status != cached_status {
        set_text(c[0], &status);
        with_ui(|u| u.status_cache = status);
    }
    if shown_login != Some(logged_in) {
        let (a, b) = if logged_in { (SW_HIDE, SW_SHOW) } else { (SW_SHOW, SW_HIDE) };
        ShowWindow(c[1], a);
        ShowWindow(c[2], a);
        ShowWindow(c[3], b);
        ShowWindow(c[21], b);
        with_ui(|u| u.logged_in_shown = Some(logged_in));
    }
    if guilds != cached_guilds {
        SendMessageW(c[3], CB_RESETCONTENT, 0, 0);
        for (_, name) in &guilds {
            let w = wide(name);
            SendMessageW(c[3], CB_ADDSTRING, 0, w.as_ptr() as isize);
        }
        with_ui(|u| u.guild_cache = guilds.clone());
    }
    if let Some(i) = guilds.iter().position(|g| g.0 == selected) {
        if SendMessageW(c[3], CB_GETCURSEL, 0, 0) != i as isize {
            SendMessageW(c[3], CB_SETCURSEL, i, 0);
        }
    }

    // Channel list: rebuild only when the text/order changed, otherwise just repaint.
    let old = with_ui(|u| std::mem::replace(&mut u.rows, rows.clone())).unwrap_or_default();
    let same_shape = old.len() == rows.len()
        && old.iter().zip(rows.iter()).all(|(a, b)| a.id == b.id && a.text == b.text && a.kind == b.kind);
    let lb = c[4];
    if !same_shape {
        let top = SendMessageW(lb, LB_GETTOPINDEX, 0, 0);
        let sel = SendMessageW(lb, LB_GETCURSEL, 0, 0);
        let sel_key = old.get(sel.max(0) as usize).filter(|_| sel >= 0).map(|r| (r.id, r.kind.clone()));
        SendMessageW(lb, WM_SETREDRAW, 0, 0);
        SendMessageW(lb, LB_RESETCONTENT, 0, 0);
        for r in &rows {
            let w = wide(&r.text);
            SendMessageW(lb, LB_ADDSTRING, 0, w.as_ptr() as isize);
        }
        if let Some((id, kind)) = sel_key {
            if let Some(i) = rows.iter().position(|r| r.id == id && r.kind == kind) {
                SendMessageW(lb, LB_SETCURSEL, i, 0);
            }
        }
        SendMessageW(lb, LB_SETTOPINDEX, top.max(0) as usize, 0);
        SendMessageW(lb, WM_SETREDRAW, 1, 0);
        InvalidateRect(lb, null(), 1);
    } else if old != rows {
        InvalidateRect(lb, null(), 0);
    }

    set_text(c[5], if selected_live() { "Watch" } else if joined { "Leave" } else { "Join" });
    SendMessageW(c[6], BM_SETCHECK, muted as usize, 0);
    SendMessageW(c[7], BM_SETCHECK, deaf as usize, 0);
    set_text(c[20], if live { "Stop Live" } else { "Go Live" });
    let pings = state().pings.len();
    set_text(c[22], &if pings > 0 { format!("Chat ({pings} @)") } else { "Chat".to_string() });
    set_text(c[9], &key_txt);
    EnableWindow(c[9], (mode == MicMode::Ptt || key_txt.starts_with("Press")) as i32);

    update_devices(&c);
    InvalidateRect(c[19], null(), 0);

    update_overlay();
    update_viewer();
    crate::ui_live::update();
    crate::ui_chat::update();
}

unsafe fn update_devices(c: &[HWND; 23]) {
    let (ins, outs, mic, spk) = {
        let s = state();
        (s.input_devices.clone(), s.output_devices.clone(), s.mic_device.clone(), s.spk_device.clone())
    };
    let Some(cache) = with_ui(|u| u.dev_cache.clone()) else { return };
    let fill = |h: HWND, label: &str, list: &[String]| {
        SendMessageW(h, CB_RESETCONTENT, 0, 0);
        let w = wide(label);
        SendMessageW(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
        for n in list {
            let w = wide(n);
            SendMessageW(h, CB_ADDSTRING, 0, w.as_ptr() as isize);
        }
    };
    if cache.0 != ins {
        fill(c[16], "Mic: Windows default", &ins);
    }
    if cache.1 != outs {
        fill(c[17], "Speakers: Windows default", &outs);
    }
    if cache.0 != ins || cache.1 != outs {
        with_ui(|u| u.dev_cache = (ins.clone(), outs.clone()));
    }
    let pick = |list: &[String], want: &Option<String>| -> usize {
        want.as_ref().and_then(|n| list.iter().position(|x| x == n)).map(|i| i + 1).unwrap_or(0)
    };
    let mi = pick(&ins, &mic);
    let si = pick(&outs, &spk);
    if SendMessageW(c[16], CB_GETCURSEL, 0, 0) != mi as isize {
        SendMessageW(c[16], CB_SETCURSEL, mi, 0);
    }
    if SendMessageW(c[17], CB_GETCURSEL, 0, 0) != si as isize {
        SendMessageW(c[17], CB_SETCURSEL, si, 0);
    }
}

unsafe fn draw_item(di: &DRAWITEMSTRUCT) {
    if di.CtlID != ID_LIST as u32 || di.itemID == u32::MAX {
        return;
    }
    let Some((row, font, bold, dpi)) =
        with_ui(|u| (u.rows.get(di.itemID as usize).cloned(), u.font, u.bold, u.dpi))
    else {
        return;
    };
    let Some(row) = row else { return };
    let hdc = di.hDC;
    let mut rc = di.rcItem;
    let selected = di.itemState & ODS_SELECTED != 0;
    let bg = if selected { rgb(204, 228, 247) } else { rgb(255, 255, 255) };
    let brush = CreateSolidBrush(bg);
    FillRect(hdc, &rc, brush);
    DeleteObject(brush as _);
    SetBkMode(hdc, TRANSPARENT as i32);

    match row.kind {
        RowKind::Chan => {
            SelectObject(hdc, bold as _);
            SetTextColor(hdc, if row.here { rgb(0, 110, 200) } else { rgb(40, 40, 40) });
            rc.left += sc(dpi, 4);
            let t = wide(&format!("{} {}", if row.here { "\u{25B6}" } else { "\u{25B8}" }, row.text));
            DrawTextW(hdc, t.as_ptr(), -1, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        }
        RowKind::Member => {
            let d = sc(dpi, 8);
            let x = rc.left + sc(dpi, 18);
            let y = (rc.top + rc.bottom - d) / 2;
            let dot = CreateSolidBrush(if row.talking { rgb(35, 190, 85) } else { rgb(200, 200, 200) });
            let old_b = SelectObject(hdc, dot as _);
            let old_p = SelectObject(hdc, GetStockObject(NULL_PEN));
            Ellipse(hdc, x, y, x + d + 1, y + d + 1);
            SelectObject(hdc, old_b);
            SelectObject(hdc, old_p);
            DeleteObject(dot as _);
            SelectObject(hdc, if row.talking { bold } else { font } as _);
            SetTextColor(
                hdc,
                if row.talking {
                    rgb(20, 150, 60)
                } else if row.dim {
                    rgb(140, 140, 140)
                } else {
                    rgb(30, 30, 30)
                },
            );
            rc.left = x + d + sc(dpi, 6);
            if row.live {
                // Red "LIVE" tag on the right, like Discord.
                let (tw, th) = (sc(dpi, 32), sc(dpi, 14));
                let tr = RECT {
                    left: rc.right - tw - sc(dpi, 6),
                    top: (rc.top + rc.bottom - th) / 2,
                    right: rc.right - sc(dpi, 6),
                    bottom: (rc.top + rc.bottom + th) / 2,
                };
                let red = CreateSolidBrush(rgb(237, 66, 69));
                let ob = SelectObject(hdc, red as _);
                let op = SelectObject(hdc, GetStockObject(NULL_PEN));
                RoundRect(hdc, tr.left, tr.top, tr.right + 1, tr.bottom + 1, sc(dpi, 6), sc(dpi, 6));
                SelectObject(hdc, ob);
                SelectObject(hdc, op);
                DeleteObject(red as _);
                SelectObject(hdc, bold as _);
                SetTextColor(hdc, rgb(255, 255, 255));
                let mut tr2 = tr;
                let t = wide("LIVE");
                DrawTextW(hdc, t.as_ptr(), -1, &mut tr2, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
                rc.right = tr.left - sc(dpi, 4);
                SelectObject(hdc, if row.talking { bold } else { font } as _);
                SetTextColor(
                    hdc,
                    if row.talking {
                        rgb(20, 150, 60)
                    } else if row.dim {
                        rgb(140, 140, 140)
                    } else {
                        rgb(30, 30, 30)
                    },
                );
            }
            let t = wide(&row.text);
            DrawTextW(hdc, t.as_ptr(), -1, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

const SENS_MIN_DB: f32 = -80.0;

/// (bar left, bar right) inside a slider of client width `w`.
fn slider_bar(dpi: i32, w: i32) -> (i32, i32) {
    (sc(dpi, 78), w - sc(dpi, 48))
}

unsafe fn slider_set_from_x(hwnd: HWND, x: i32) {
    let dpi = with_ui(|u| u.dpi).unwrap_or(96);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let (l, r) = slider_bar(dpi, rc.right);
    let frac = ((x - l) as f32 / (r - l).max(1) as f32).clamp(0.0, 1.0);
    let sh = crate::voice::shared();
    let id = GetDlgCtrlID(hwnd);
    if id == ID_SENS {
        let db = SENS_MIN_DB + frac * -SENS_MIN_DB;
        sh.vad_db.store(db.round().to_bits(), Ordering::Relaxed);
    } else if id == ID_NSLVL {
        // 0% = off; otherwise RNNoise at that strength.
        let k = (frac * 20.0).round() / 20.0; // 5% steps
        sh.noise_suppression.store(k > 0.0, Ordering::Relaxed);
        if k > 0.0 {
            sh.ns_strength.store(k.to_bits(), Ordering::Relaxed);
        }
    } else {
        let mut v = frac * 2.0;
        if (v - 1.0).abs() < 0.04 {
            v = 1.0; // easy to land back on 100%
        }
        sh.input_volume.store(v.to_bits(), Ordering::Relaxed);
    }
    InvalidateRect(hwnd, null(), 0);
}

/// Discord-style slider. Sensitivity: the bar shows your live mic level
/// (green = loud enough to send, yellow = below the line) and the handle is
/// the line. Input volume: 0-200%.
unsafe extern "system" fn slider_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let x_of = |lp: LPARAM| (lp & 0xFFFF) as u16 as i16 as i32;
    match msg {
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            SetCapture(hwnd);
            slider_set_from_x(hwnd, x_of(lp));
            0
        }
        WM_MOUSEMOVE => {
            if GetCapture() == hwnd {
                slider_set_from_x(hwnd, x_of(lp));
            }
            0
        }
        WM_LBUTTONUP => {
            if GetCapture() == hwnd {
                ReleaseCapture();
                store::save_settings();
            }
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let mem = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let old_bmp = SelectObject(mem, bmp as _);
            FillRect(mem, &rc, GetSysColorBrush(COLOR_BTNFACE));
            let (dpi, font) = with_ui(|u| (u.dpi, u.font)).unwrap_or((96, null_mut()));
            SelectObject(mem, font as _);
            SetBkMode(mem, TRANSPARENT as i32);
            SetTextColor(mem, rgb(30, 30, 30));

            let sh = crate::voice::shared();
            let id = GetDlgCtrlID(hwnd);
            let is_sens = id == ID_SENS;
            let (l, r) = slider_bar(dpi, rc.right);
            let bh = sc(dpi, 10);
            let top = (rc.bottom - bh) / 2;
            let fill = |x0: i32, x1: i32, color: COLORREF| {
                let b = CreateSolidBrush(color);
                let rr = RECT { left: x0, top, right: x1.max(x0), bottom: top + bh };
                FillRect(mem, &rr, b);
                DeleteObject(b as _);
            };
            fill(l, r, rgb(205, 205, 210));
            let (label, value, frac) = if is_sens {
                let thr = f32::from_bits(sh.vad_db.load(Ordering::Relaxed));
                let live = { let s = state(); s.joined.is_some() && s.voice_ready };
                if live {
                    let lvl = f32::from_bits(sh.vad_level.load(Ordering::Relaxed));
                    let lf = ((lvl - SENS_MIN_DB) / -SENS_MIN_DB).clamp(0.0, 1.0);
                    let color = if lvl > thr { rgb(60, 190, 90) } else { rgb(235, 185, 40) };
                    fill(l, l + ((r - l) as f32 * lf) as i32, color);
                }
                ("Sensitivity", format!("{:.0} dB", thr), (thr - SENS_MIN_DB) / -SENS_MIN_DB)
            } else if id == ID_NSLVL {
                let on = sh.noise_suppression.load(Ordering::Relaxed);
                let k = if on { f32::from_bits(sh.ns_strength.load(Ordering::Relaxed)) } else { 0.0 };
                fill(l, l + ((r - l) as f32 * k) as i32, rgb(140, 100, 200));
                ("Noise removal", if on { format!("{:.0}%", k * 100.0) } else { "off".into() }, k)
            } else {
                let v = f32::from_bits(sh.input_volume.load(Ordering::Relaxed));
                fill(l, l + ((r - l) as f32 * v / 2.0) as i32, rgb(70, 130, 220));
                ("Input volume", format!("{:.0}%", v * 100.0), v / 2.0)
            };
            // Handle
            let hx = l + ((r - l) as f32 * frac.clamp(0.0, 1.0)) as i32;
            let hw = sc(dpi, 3);
            let b = CreateSolidBrush(rgb(40, 40, 45));
            let hr = RECT { left: hx - hw, top: sc(dpi, 2), right: hx + hw, bottom: rc.bottom - sc(dpi, 2) };
            FillRect(mem, &hr, b);
            DeleteObject(b as _);

            let mut lr = RECT { left: sc(dpi, 2), top: 0, right: l - sc(dpi, 4), bottom: rc.bottom };
            let t = wide(label);
            DrawTextW(mem, t.as_ptr(), -1, &mut lr, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            let mut vr = RECT { left: r + sc(dpi, 6), top: 0, right: rc.right, bottom: rc.bottom };
            let t = wide(&value);
            DrawTextW(mem, t.as_ptr(), -1, &mut vr, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);

            BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old_bmp);
            DeleteObject(bmp as _);
            DeleteDC(mem);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------
// Stream viewer window
// ---------------------------------------------------------------------------

static VIEW_HWND: std::sync::atomic::AtomicPtr<core::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(null_mut());

thread_local! {
    /// Saved placement while the viewer is fullscreen.
    static VIEW_FULL: std::cell::Cell<Option<WINDOWPLACEMENT>> = const { std::cell::Cell::new(None) };
    static VIEW_UI: RefCell<ViewUi> = RefCell::new(ViewUi::default());
}

/// Viewer control bar state (UI thread only).
struct ViewUi {
    bar: bool,
    last_move: std::time::Instant,
    dragging: bool,
    /// Volume before M muted the stream.
    unmute_to: Option<f32>,
}

impl Default for ViewUi {
    fn default() -> Self {
        Self { bar: true, last_move: std::time::Instant::now(), dragging: false, unmute_to: None }
    }
}

const VIEW_TIMER: usize = 1;

/// Called from the video thread when a new picture is ready.
fn view_repaint() {
    let h = VIEW_HWND.load(Ordering::Acquire);
    if !h.is_null() {
        unsafe { InvalidateRect(h, null(), 0) };
    }
}

fn stream_volume() -> f32 {
    f32::from_bits(crate::video::STREAM_VOLUME.load(Ordering::Relaxed))
}

fn set_stream_volume(v: f32) {
    let mut v = v.clamp(0.0, 2.0);
    if (v - 1.0).abs() < 0.03 {
        v = 1.0; // easy to land back on 100%
    }
    crate::video::STREAM_VOLUME.store(v.to_bits(), Ordering::Relaxed);
}

fn is_pinned() -> bool {
    crate::video::STREAM_PIN.load(Ordering::Relaxed)
}

unsafe fn apply_pin(hwnd: HWND) {
    let after = if is_pinned() { HWND_TOPMOST } else { HWND_NOTOPMOST };
    SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

unsafe fn toggle_pin(hwnd: HWND) {
    crate::video::STREAM_PIN.store(!is_pinned(), Ordering::Relaxed);
    apply_pin(hwnd);
    store::save_settings();
    InvalidateRect(hwnd, null(), 0);
}

/// Open or close the viewer to match `state().watching`.
unsafe fn update_viewer() {
    let watching = state().watching.clone();
    let h = VIEW_HWND.load(Ordering::Acquire);
    match (watching, h.is_null()) {
        (Some(w), true) => {
            let dpi = with_ui(|u| u.dpi).unwrap_or(96);
            let style = WS_OVERLAPPEDWINDOW;
            let mut r = RECT { left: 0, top: 0, right: sc(dpi, 960), bottom: sc(dpi, 540) };
            AdjustWindowRect(&mut r, style, 0);
            let cls = wide("tinycord_view");
            let title = wide(&format!("Watching {}", w.name));
            let hwnd = CreateWindowExW(
                0,
                cls.as_ptr(),
                title.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                r.right - r.left,
                r.bottom - r.top,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            VIEW_HWND.store(hwnd, Ordering::Release);
            VIEW_UI.with(|v| *v.borrow_mut() = ViewUi::default());
            ShowWindow(hwnd, SW_SHOW);
            apply_pin(hwnd);
            SetTimer(hwnd, VIEW_TIMER, 250, None);
        }
        (None, false) => {
            VIEW_HWND.store(null_mut(), Ordering::Release);
            DestroyWindow(h);
        }
        (Some(_), false) => {
            InvalidateRect(h, null(), 0);
        }
        (None, true) => {}
    }
}

unsafe fn is_fullscreen() -> bool {
    VIEW_FULL.with(|f| {
        let v = f.take();
        let some = v.is_some();
        f.set(v);
        some
    })
}

unsafe fn toggle_fullscreen(hwnd: HWND) {
    if let Some(wp) = VIEW_FULL.with(|f| f.take()) {
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_OVERLAPPEDWINDOW | WS_VISIBLE) as isize);
        SetWindowPlacement(hwnd, &wp);
        let after = if is_pinned() { HWND_TOPMOST } else { HWND_NOTOPMOST };
        SetWindowPos(hwnd, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED);
    } else {
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        GetWindowPlacement(hwnd, &mut wp);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        VIEW_FULL.with(|f| f.set(Some(wp)));
        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE) as isize);
        let m = mi.rcMonitor;
        SetWindowPos(hwnd, HWND_TOP, m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_FRAMECHANGED);
    }
    InvalidateRect(hwnd, null(), 0);
}

/// Where the control bar's parts are, for a `cw`x`ch` client area.
struct Bar {
    bar: RECT,
    pin: RECT,
    label: RECT,
    track: RECT,
    value: RECT,
    full: RECT,
}

fn bar_layout(dpi: i32, cw: i32, ch: i32) -> Bar {
    let h = sc(dpi, 38);
    let top = ch - h;
    let pad = sc(dpi, 8);
    let by = top + sc(dpi, 6);
    let bb = ch - sc(dpi, 6);
    let pin = RECT { left: pad, top: by, right: pad + sc(dpi, 76), bottom: bb };
    let full = RECT { left: cw - pad - sc(dpi, 96), top: by, right: cw - pad, bottom: bb };
    let label = RECT { left: pin.right + sc(dpi, 14), top, right: pin.right + sc(dpi, 70), bottom: ch };
    let value = RECT { left: full.left - sc(dpi, 58), top, right: full.left - sc(dpi, 10), bottom: ch };
    let mid = top + h / 2;
    let track = RECT {
        left: label.right + sc(dpi, 4),
        top: mid - sc(dpi, 3),
        right: (value.left - sc(dpi, 10)).max(label.right + sc(dpi, 44)),
        bottom: mid + sc(dpi, 3),
    };
    Bar { bar: RECT { left: 0, top, right: cw, bottom: ch }, pin, label, track, value, full }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

unsafe fn view_bar(hwnd: HWND) -> Bar {
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    bar_layout(with_ui(|u| u.dpi).unwrap_or(96), rc.right, rc.bottom)
}

unsafe fn volume_from_x(hwnd: HWND, x: i32) {
    let b = view_bar(hwnd);
    let frac = (x - b.track.left) as f32 / (b.track.right - b.track.left).max(1) as f32;
    set_stream_volume(frac.clamp(0.0, 1.0) * 2.0);
    VIEW_UI.with(|v| v.borrow_mut().unmute_to = None);
    InvalidateRect(hwnd, null(), 0);
}

/// Mouse button down (or double-click) at x,y. Returns true if the bar took it.
unsafe fn bar_click(hwnd: HWND, x: i32, y: i32) -> bool {
    if !VIEW_UI.with(|v| v.borrow().bar) {
        return false;
    }
    let b = view_bar(hwnd);
    if !inside(&b.bar, x, y) {
        return false;
    }
    if inside(&b.pin, x, y) {
        toggle_pin(hwnd);
    } else if inside(&b.full, x, y) {
        toggle_fullscreen(hwnd);
    } else if x >= b.track.left - 8 && x <= b.track.right + 8 {
        VIEW_UI.with(|v| v.borrow_mut().dragging = true);
        SetCapture(hwnd);
        volume_from_x(hwnd, x);
    }
    true
}

unsafe fn paint_bar(hdc: HDC, b: &Bar, dpi: i32) {
    let fill = |r: &RECT, c: COLORREF| {
        let br = CreateSolidBrush(c);
        FillRect(hdc, r, br);
        DeleteObject(br as _);
    };
    let button = |r: &RECT, c: COLORREF, text: &str| {
        let br = CreateSolidBrush(c);
        let ob = SelectObject(hdc, br as _);
        let op = SelectObject(hdc, GetStockObject(NULL_PEN));
        RoundRect(hdc, r.left, r.top, r.right + 1, r.bottom + 1, sc(dpi, 8), sc(dpi, 8));
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        DeleteObject(br as _);
        let mut rr = *r;
        let t = wide(text);
        SetTextColor(hdc, rgb(255, 255, 255));
        DrawTextW(hdc, t.as_ptr(), -1, &mut rr, DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX);
    };
    let accent = rgb(88, 101, 242);
    let grey = rgb(64, 66, 73);
    fill(&b.bar, rgb(30, 31, 34));
    let font = with_ui(|u| u.bold).unwrap_or(null_mut());
    SelectObject(hdc, font as _);
    SetBkMode(hdc, TRANSPARENT as i32);
    let pinned = is_pinned();
    button(&b.pin, if pinned { accent } else { grey }, if pinned { "Pinned" } else { "Pin" });
    button(&b.full, grey, if is_fullscreen() { "Exit full" } else { "Fullscreen" });

    SetTextColor(hdc, rgb(200, 200, 205));
    let mut lr = b.label;
    let t = wide("Volume");
    DrawTextW(hdc, t.as_ptr(), -1, &mut lr, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    let vol = stream_volume();
    let mut vr = b.value;
    let t = wide(&if vol == 0.0 { "Muted".to_string() } else { format!("{:.0}%", vol * 100.0) });
    DrawTextW(hdc, t.as_ptr(), -1, &mut vr, DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX);

    let tr = b.track;
    fill(&tr, rgb(78, 80, 88));
    let x = tr.left + ((tr.right - tr.left) as f32 * vol / 2.0) as i32;
    fill(&RECT { left: tr.left, top: tr.top, right: x, bottom: tr.bottom }, accent);
    // Mark at 100%.
    let mid = (tr.left + tr.right) / 2;
    fill(&RECT { left: mid, top: tr.top - sc(dpi, 3), right: mid + 1, bottom: tr.bottom + sc(dpi, 3) }, rgb(140, 140, 150));
    let hw = sc(dpi, 4);
    fill(&RECT { left: x - hw, top: tr.top - sc(dpi, 6), right: x + hw, bottom: tr.bottom + sc(dpi, 6) }, rgb(255, 255, 255));
}

unsafe extern "system" fn view_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let xy = |lp: LPARAM| ((lp & 0xFFFF) as u16 as i16 as i32, ((lp >> 16) & 0xFFFF) as u16 as i16 as i32);
    match msg {
        WM_SIZE => {
            let w = (lp & 0xFFFF) as u32;
            let h = ((lp >> 16) & 0xFFFF) as u32;
            if w > 0 && h > 0 {
                crate::video::TARGET_W.store(w, Ordering::Relaxed);
                crate::video::TARGET_H.store(h, Ordering::Relaxed);
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            // Hide the bar after 2.5 s without mouse movement (not while
            // the mouse is on it, dragging, or there's no picture yet).
            let has_pic = !crate::video::rendered().lock().px.is_empty();
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
            let over_bar = inside(&view_bar(hwnd).bar, pt.x, pt.y);
            let hide = VIEW_UI.with(|v| {
                let v = v.borrow();
                v.bar && has_pic && !v.dragging && !over_bar && v.last_move.elapsed().as_millis() > 2500
            });
            if hide {
                VIEW_UI.with(|v| v.borrow_mut().bar = false);
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSEMOVE => {
            let (x, _) = xy(lp);
            let (was, dragging) = VIEW_UI.with(|v| {
                let mut v = v.borrow_mut();
                v.last_move = std::time::Instant::now();
                let was = v.bar;
                v.bar = true;
                (was, v.dragging)
            });
            if dragging {
                volume_from_x(hwnd, x);
            } else if !was {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_SETCURSOR => {
            // Hide the mouse pointer in fullscreen once the bar is hidden.
            let hidden = !VIEW_UI.with(|v| v.borrow().bar);
            if hidden && is_fullscreen() && (lp & 0xFFFF) as u32 == HTCLIENT {
                SetCursor(null_mut());
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            bar_click(hwnd, x, y);
            0
        }
        WM_LBUTTONUP => {
            if VIEW_UI.with(|v| std::mem::take(&mut v.borrow_mut().dragging)) {
                ReleaseCapture();
                store::save_settings();
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = xy(lp);
            if !bar_click(hwnd, x, y) {
                toggle_fullscreen(hwnd);
            }
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp >> 16) & 0xFFFF) as u16 as i16 as f32 / 120.0;
            set_stream_volume(stream_volume() + delta * 0.05);
            VIEW_UI.with(|v| {
                let mut v = v.borrow_mut();
                v.bar = true;
                v.last_move = std::time::Instant::now();
                v.unmute_to = None;
            });
            store::save_settings();
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_KEYDOWN => {
            match wp as u32 {
                0x1B if is_fullscreen() => toggle_fullscreen(hwnd), // Esc
                0x7A => toggle_fullscreen(hwnd),                     // F11
                0x54 => toggle_pin(hwnd),                            // T / pin
                0x26 | 0x28 => {
                    // Up / Down: volume
                    let step = if wp as u32 == 0x26 { 0.05 } else { -0.05 };
                    set_stream_volume(stream_volume() + step);
                    store::save_settings();
                    VIEW_UI.with(|v| {
                        let mut v = v.borrow_mut();
                        v.bar = true;
                        v.last_move = std::time::Instant::now();
                    });
                    InvalidateRect(hwnd, null(), 0);
                }
                0x4D => {
                    // M: mute / unmute the stream
                    let back = VIEW_UI.with(|v| v.borrow_mut().unmute_to.take());
                    match back {
                        Some(v) => set_stream_volume(v),
                        None => {
                            let cur = stream_volume();
                            VIEW_UI.with(|v| v.borrow_mut().unmute_to = Some(if cur > 0.0 { cur } else { 1.0 }));
                            set_stream_volume(0.0);
                        }
                    }
                    store::save_settings();
                    VIEW_UI.with(|v| {
                        let mut v = v.borrow_mut();
                        v.bar = true;
                        v.last_move = std::time::Instant::now();
                    });
                    InvalidateRect(hwnd, null(), 0);
                }
                _ => {}
            }
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let dpi = with_ui(|u| u.dpi).unwrap_or(96);
            let show_bar = VIEW_UI.with(|v| v.borrow().bar);
            let bar = bar_layout(dpi, rc.right, rc.bottom);
            if show_bar {
                // Keep the picture from drawing under the bar (no flicker).
                ExcludeClipRect(hdc, bar.bar.left, bar.bar.top, bar.bar.right, bar.bar.bottom);
            }
            let black = GetStockObject(BLACK_BRUSH) as HBRUSH;
            let pic = crate::video::rendered().lock();
            let status = crate::video::info().lock().1.clone();
            if pic.w > 0 && !pic.px.is_empty() {
                let (w, h) = (pic.w as i32, pic.h as i32);
                let x0 = ((rc.right - w) / 2).max(0);
                let y0 = ((rc.bottom - h) / 2).max(0);
                // Black bars around the picture.
                for r in [
                    RECT { left: 0, top: 0, right: rc.right, bottom: y0 },
                    RECT { left: 0, top: y0 + h, right: rc.right, bottom: rc.bottom },
                    RECT { left: 0, top: y0, right: x0, bottom: y0 + h },
                    RECT { left: x0 + w, top: y0, right: rc.right, bottom: y0 + h },
                ] {
                    if r.right > r.left && r.bottom > r.top {
                        FillRect(hdc, &r, black);
                    }
                }
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = w;
                bi.bmiHeader.biHeight = -h; // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = BI_RGB;
                SetDIBitsToDevice(
                    hdc,
                    x0,
                    y0,
                    w as u32,
                    h as u32,
                    0,
                    0,
                    0,
                    h as u32,
                    pic.px.as_ptr() as *const _,
                    &bi,
                    DIB_RGB_COLORS,
                );
            } else {
                FillRect(hdc, &rc, black);
            }
            drop(pic);
            if !status.is_empty() {
                let font = with_ui(|u| u.ofont).unwrap_or(null_mut());
                SelectObject(hdc, font as _);
                SetBkMode(hdc, TRANSPARENT as i32);
                SetTextColor(hdc, rgb(220, 220, 225));
                let mut tr = RECT { left: sc(dpi, 12), top: sc(dpi, 10), right: rc.right - sc(dpi, 12), bottom: bar.bar.top };
                let t = wide(&status);
                DrawTextW(hdc, t.as_ptr(), -1, &mut tr, DT_WORDBREAK | DT_NOPREFIX);
            }
            if show_bar {
                SelectClipRgn(hdc, null_mut());
                paint_bar(hdc, &bar, dpi);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_CLOSE => {
            // Close right away; the network side catches up.
            KillTimer(hwnd, VIEW_TIMER);
            state().watching = None;
            state::send(Cmd::StopWatch);
            VIEW_HWND.store(null_mut(), Ordering::Release);
            VIEW_FULL.with(|f| f.set(None));
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Lines for the overlay: (name, talking, dim).
fn overlay_lines() -> Vec<(String, bool, bool)> {
    let s = state();
    if s.overlay == 0 {
        return Vec::new();
    }
    let Some((g, c)) = s.joined else { return Vec::new() };
    s.members_of(g, c)
        .into_iter()
        .map(|(uid, name, vs)| (name, s.is_speaking(uid), vs.mute || vs.deaf))
        .filter(|(_, talking, _)| *talking || !s.overlay_speakers_only)
        .collect()
}

unsafe fn update_overlay() {
    let lines = overlay_lines();
    let Some((ov, old, ofont, dpi)) = with_ui(|u| (u.overlay, u.overlay_lines.clone(), u.ofont, u.dpi)) else {
        return;
    };
    if lines == old {
        return;
    }
    with_ui(|u| u.overlay_lines = lines.clone());
    if lines.is_empty() {
        ShowWindow(ov, SW_HIDE);
        return;
    }
    // Measure the longest name.
    let hdc = GetDC(ov);
    let prev = SelectObject(hdc, ofont as _);
    let mut w = 0;
    for (name, _, _) in &lines {
        let t: Vec<u16> = name.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(hdc, t.as_ptr(), t.len() as i32, &mut sz);
        w = w.max(sz.cx);
    }
    SelectObject(hdc, prev);
    ReleaseDC(ov, hdc);

    let line_h = sc(dpi, 22);
    let width = w + sc(dpi, 34);
    let height = line_h * lines.len() as i32 + sc(dpi, 8);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let m = sc(dpi, 12);
    let corner = state().overlay;
    let (x, y) = match corner {
        1 => (m, m),
        2 => (sw - width - m, m),
        3 => (m, sh - height - m),
        4 => (sw - width - m, sh - height - m),
        _ => (m, (sh - height) / 2),
    };
    SetWindowPos(ov, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    InvalidateRect(ov, null(), 0);
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            // Draw off-screen, then copy once (no flicker).
            let mem = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
            let old_bmp = SelectObject(mem, bmp as _);
            let bg = CreateSolidBrush(rgb(18, 18, 22));
            FillRect(mem, &rc, bg);
            DeleteObject(bg as _);
            if let Some((lines, ofont, dpi)) = with_ui(|u| (u.overlay_lines.clone(), u.ofont, u.dpi)) {
                SelectObject(mem, ofont as _);
                SetBkMode(mem, TRANSPARENT as i32);
                let line_h = sc(dpi, 22);
                let d = sc(dpi, 9);
                for (i, (name, talking, dim)) in lines.iter().enumerate() {
                    let top = sc(dpi, 4) + line_h * i as i32;
                    let x = sc(dpi, 8);
                    let y = top + (line_h - d) / 2;
                    let dot = CreateSolidBrush(if *talking { rgb(60, 235, 110) } else { rgb(90, 90, 96) });
                    let ob = SelectObject(mem, dot as _);
                    let op = SelectObject(mem, GetStockObject(NULL_PEN));
                    Ellipse(mem, x, y, x + d + 1, y + d + 1);
                    SelectObject(mem, ob);
                    SelectObject(mem, op);
                    DeleteObject(dot as _);
                    SetTextColor(
                        mem,
                        if *talking {
                            rgb(110, 255, 150)
                        } else if *dim {
                            rgb(130, 130, 136)
                        } else {
                            rgb(215, 215, 220)
                        },
                    );
                    let mut r = RECT { left: x + d + sc(dpi, 8), top, right: rc.right - sc(dpi, 4), bottom: top + line_h };
                    let t = wide(name);
                    DrawTextW(mem, t.as_ptr(), -1, &mut r, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
                }
            }
            BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old_bmp);
            DeleteObject(bmp as _);
            DeleteDC(mem);
            EndPaint(hwnd, &ps);
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as isize,
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

