//! Token and settings on disk, in %APPDATA%\tinycord.
//! On Windows the token is encrypted with DPAPI, so only your Windows
//! account on this PC can read it back.

use crate::state::{state, MicMode};
use std::path::PathBuf;

fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    let d = base.join("tinycord");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn load_token() -> Option<String> {
    let raw = std::fs::read(dir().join("token.bin")).ok()?;
    let plain = unprotect(&raw)?;
    let t = String::from_utf8(plain).ok()?;
    let t = t.trim().to_string();
    (!t.is_empty()).then_some(t)
}

pub fn save_token(token: &str) {
    if let Some(enc) = protect(token.as_bytes()) {
        let _ = std::fs::write(dir().join("token.bin"), enc);
    }
}

pub fn clear_token() {
    let _ = std::fs::remove_file(dir().join("token.bin"));
}

pub fn load_settings() {
    let Ok(text) = std::fs::read_to_string(dir().join("settings.txt")) else { return };
    let mut s = state();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "mode" => {
                s.mode = match v.trim() {
                    "open" => MicMode::Open,
                    "ptt" => MicMode::Ptt,
                    _ => MicMode::Voice,
                }
            }
            "ptt_key" => s.ptt_key = v.trim().parse().unwrap_or(0),
            "overlay" => s.overlay = v.trim().parse().unwrap_or(2).min(4),
            "speakers_only" => s.overlay_speakers_only = v.trim() == "1",
            "guild" => s.selected_guild = v.trim().parse().unwrap_or(0),
            "vad_db" => {
                if let Ok(db) = v.trim().parse::<f32>() {
                    crate::voice::shared()
                        .vad_db
                        .store(db.clamp(-70.0, -10.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}

pub fn save_settings() {
    let s = state();
    let vad = f32::from_bits(
        crate::voice::shared().vad_db.load(std::sync::atomic::Ordering::Relaxed),
    );
    let text = format!(
        "mode={}\nptt_key={}\noverlay={}\nspeakers_only={}\nguild={}\nvad_db={}\n",
        match s.mode {
            MicMode::Open => "open",
            MicMode::Voice => "voice",
            MicMode::Ptt => "ptt",
        },
        s.ptt_key,
        s.overlay,
        s.overlay_speakers_only as u8,
        s.selected_guild,
        vad,
    );
    drop(s);
    let _ = std::fs::write(dir().join("settings.txt"), text);
}

#[cfg(any(windows, wincheck))]
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        );
        if ok == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(v)
    }
}

#[cfg(any(windows, wincheck))]
fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        );
        if ok == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as _);
        Some(v)
    }
}

// Non-Windows builds are only for development checks.
#[cfg(not(any(windows, wincheck)))]
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    Some(data.to_vec())
}

#[cfg(not(any(windows, wincheck)))]
fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    Some(data.to_vec())
}
