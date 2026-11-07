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
const SENS: [(&str, f32); 3] = [("Mic sens: high", -55.0), ("Mic sens: medium", -45.0), ("Mic sens: low", -35.0)];

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
}

struct Ui {
    hwnd: HWND,
    overlay: HWND,
    c: [HWND; 14],
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
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.borrow_mut().as_mut().map(f))
}

fn sc(dpi: i32, v: i32) -> i32 {
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

unsafe fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    SetWindowTextW(h, w.as_ptr());
}

unsafe fn get_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h);
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, buf.as_mut_ptr(), n + 1);
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

unsafe fn make_font(dpi: i32, pt: i32, weight: i32) -> HFONT {
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

        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                hwnd: null_mut(),
                overlay: null_mut(),
                c: [null_mut(); 14],
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
            })
        });

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: sc(dpi, 280), bottom: sc(dpi, 434) };
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

        ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, TIMER_REFRESH, 50, None);
        SetTimer(hwnd, TIMER_TOPMOST, 2000, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe fn child(
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

unsafe fn combo_add(h: HWND, items: &[&str]) {
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
    let mut c = [null_mut(); 14];
    c[0] = child(hwnd, "STATIC", "Starting...", SS_LEFT as u32 | SS_ENDELLIPSIS as u32, 6, 6, 268, 18, ID_STATUS, dpi, font);
    c[1] = child(hwnd, "EDIT", "", ts | ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32, 6, 28, 190, 24, ID_TOKEN, dpi, font);
    c[2] = b("Log in", BS_PUSHBUTTON, 202, 28, 72, 24, ID_LOGIN);
    c[3] = cbo(6, 28, 268, ID_GUILD);
    c[4] = child(
        hwnd,
        "LISTBOX",
        "",
        ts | WS_VSCROLL | (LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT) as u32,
        6,
        58,
        268,
        250,
        ID_LIST,
        dpi,
        font,
    );
    c[5] = b("Join", BS_PUSHBUTTON, 6, 314, 86, 26, ID_JOIN);
    c[6] = b("Mute", BS_AUTOCHECKBOX | BS_PUSHLIKE, 97, 314, 86, 26, ID_MUTE);
    c[7] = b("Deafen", BS_AUTOCHECKBOX | BS_PUSHLIKE, 188, 314, 86, 26, ID_DEAF);
    c[8] = cbo(6, 346, 130, ID_MODE);
    c[9] = b("PTT key: not set", BS_PUSHBUTTON, 142, 345, 132, 25, ID_KEY);
    c[10] = cbo(6, 376, 130, ID_SENS);
    c[11] = cbo(142, 376, 132, ID_OVERLAY);
    c[12] = b("Speakers only", BS_AUTOCHECKBOX, 8, 406, 128, 22, ID_SPK);
    c[13] = b("Keep on top", BS_AUTOCHECKBOX, 144, 406, 128, 22, ID_TOP);

    combo_add(c[8], &["Voice activation", "Push-to-talk", "Open mic"]);
    combo_add(c[10], &SENS.map(|s| s.0));
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
    drop(s);
    let vad = f32::from_bits(crate::voice::shared().vad_db.load(Ordering::Relaxed));
    let sens_idx = SENS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 .1 - vad).abs().total_cmp(&(b.1 .1 - vad).abs()))
        .map(|x| x.0)
        .unwrap_or(1);
    SendMessageW(c[10], CB_SETCURSEL, sens_idx, 0);

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
        WM_DESTROY => {
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
        (ID_JOIN, BN_CLICKED) => {
            if state().joined.is_some() {
                state::send(Cmd::Leave);
            } else {
                join_selected();
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
        (ID_SENS, CBN_SELCHANGE) => {
            let i = SendMessageW(c[10], CB_GETCURSEL, 0, 0) as usize;
            if let Some(s) = SENS.get(i) {
                crate::voice::shared().vad_db.store(s.1.to_bits(), Ordering::Relaxed);
                store::save_settings();
            }
        }
        (ID_OVERLAY, CBN_SELCHANGE) => {
            let i = SendMessageW(c[11], CB_GETCURSEL, 0, 0);
            state().overlay = i.clamp(0, 5) as u8;
            store::save_settings();
            with_ui(|u| u.overlay_lines.clear());
            state::changed();
        }
        (ID_SPK, BN_CLICKED) => {
            state().overlay_speakers_only = SendMessageW(c[12], BM_GETCHECK, 0, 0) == 1;
            store::save_settings();
            state::changed();
        }
        (ID_TOP, BN_CLICKED) => {
            let on = SendMessageW(c[13], BM_GETCHECK, 0, 0) == 1;
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

unsafe fn join_selected() {
    let Some((lb, rows)) = with_ui(|u| (u.c[4], u.rows.clone())) else { return };
    let i = SendMessageW(lb, LB_GETCURSEL, 0, 0);
    if i < 0 {
        return;
    }
    let Some(row) = rows.get(i as usize) else { return };
    let g = state().selected_guild;
    if g != 0 && row.channel != 0 {
        state::send(Cmd::Join(g, row.channel));
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

    set_text(c[5], if joined { "Leave" } else { "Join" });
    SendMessageW(c[6], BM_SETCHECK, muted as usize, 0);
    SendMessageW(c[7], BM_SETCHECK, deaf as usize, 0);
    set_text(c[9], &key_txt);
    EnableWindow(c[9], (mode == MicMode::Ptt || key_txt.starts_with("Press")) as i32);

    update_overlay();
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
            let t = wide(&row.text);
            DrawTextW(hdc, t.as_ptr(), -1, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
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

