//! Token and settings on disk, in %APPDATA%\tinycord.
//! On Windows the token is encrypted with DPAPI, so only your Windows
//! account on this PC can read it back.

use crate::state::{state, MicMode};
use std::path::PathBuf;

pub fn dir() -> PathBuf {
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
            "overlay" => s.overlay = v.trim().parse().unwrap_or(2).min(crate::state::OVERLAY_MAX),
            "on_top" => s.keep_on_top = v.trim() == "1",
            "speakers_only" => s.overlay_speakers_only = v.trim() == "1",
            "guild" => s.selected_guild = v.trim().parse().unwrap_or(0),
            "mic" => {
                s.mic_chosen = true;
                s.mic_device = Some(v.trim().to_string()).filter(|x| !x.is_empty());
            }
            "speakers" => s.spk_device = Some(v.trim().to_string()).filter(|x| !x.is_empty()),
            "noise_suppression" => crate::voice::shared()
                .noise_suppression
                .store(v.trim() != "0", std::sync::atomic::Ordering::Relaxed),
            "auto_gain" => crate::voice::shared()
                .auto_gain
                .store(v.trim() != "0", std::sync::atomic::Ordering::Relaxed),
            "stream_volume" => {
                if let Ok(x) = v.trim().parse::<f32>() {
                    crate::video::STREAM_VOLUME
                        .store(x.clamp(0.0, 2.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
            }
            "stream_pin" => crate::video::STREAM_PIN.store(v.trim() == "1", std::sync::atomic::Ordering::Relaxed),
            "live_monitor" => s.live_cfg.monitor = v.trim().parse().unwrap_or(usize::MAX),
            "live_res" => s.live_cfg.height = v.trim().parse().unwrap_or(720),
            "live_fps" => s.live_cfg.fps = v.trim().parse().unwrap_or(30),
            "live_sound" => s.live_cfg.sound = v.trim() != "0",
            "input_volume" => {
                if let Ok(x) = v.trim().parse::<f32>() {
                    crate::voice::shared()
                        .input_volume
                        .store(x.clamp(0.0, 2.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
            }
            "ns_strength" => {
                if let Ok(x) = v.trim().parse::<f32>() {
                    crate::voice::shared()
                        .ns_strength
                        .store(x.clamp(0.05, 1.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
                }
            }
            "clarity" => crate::voice::shared()
                .clarity
                .store(v.trim() == "1", std::sync::atomic::Ordering::Relaxed),
            "vad_db" => {
                if let Ok(db) = v.trim().parse::<f32>() {
                    crate::voice::shared()
                        .vad_db
                        .store(db.clamp(-80.0, 0.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
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
        "mode={}\nptt_key={}\noverlay={}\nspeakers_only={}\nguild={}\nvad_db={}\nnoise_suppression={}\nauto_gain={}\n{}speakers={}\ninput_volume={}\nns_strength={}\nclarity={}\nstream_volume={}\nstream_pin={}\nlive_monitor={}\nlive_res={}\nlive_fps={}\nlive_sound={}\non_top={}\n",
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
        crate::voice::shared().noise_suppression.load(std::sync::atomic::Ordering::Relaxed) as u8,
        crate::voice::shared().auto_gain.load(std::sync::atomic::Ordering::Relaxed) as u8,
        if s.mic_chosen { format!("mic={}\n", s.mic_device.clone().unwrap_or_default()) } else { String::new() },
        s.spk_device.clone().unwrap_or_default(),
        f32::from_bits(crate::voice::shared().input_volume.load(std::sync::atomic::Ordering::Relaxed)),
        f32::from_bits(crate::voice::shared().ns_strength.load(std::sync::atomic::Ordering::Relaxed)),
        crate::voice::shared().clarity.load(std::sync::atomic::Ordering::Relaxed) as u8,
        f32::from_bits(crate::video::STREAM_VOLUME.load(std::sync::atomic::Ordering::Relaxed)),
        crate::video::STREAM_PIN.load(std::sync::atomic::Ordering::Relaxed) as u8,
        s.live_cfg.monitor,
        s.live_cfg.height,
        s.live_cfg.fps,
        s.live_cfg.sound as u8,
        s.keep_on_top as u8,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LiveCfg;

    /// Every setting comes back the way it was saved.
    #[test]
    fn settings_round_trip() {
        let tmp = std::env::temp_dir().join(format!("tinycord-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("APPDATA", &tmp);
        let cfg = LiveCfg { monitor: 1, height: 1080, fps: 60, sound: false };
        for overlay in 0..=crate::state::OVERLAY_MAX {
            {
                let mut s = state();
                s.overlay = overlay;
                s.overlay_speakers_only = true;
                s.keep_on_top = true;
                s.mode = MicMode::Ptt;
                s.ptt_key = 5;
                s.live_cfg = cfg;
            }
            save_settings();
            {
                let mut s = state();
                s.overlay = 2;
                s.overlay_speakers_only = false;
                s.keep_on_top = false;
                s.mode = MicMode::Voice;
                s.ptt_key = 0;
                s.live_cfg = LiveCfg::default();
            }
            load_settings();
            let s = state();
            assert_eq!(s.overlay, overlay, "overlay position");
            assert!(s.overlay_speakers_only && s.keep_on_top);
            assert_eq!((s.mode, s.ptt_key), (MicMode::Ptt, 5));
            assert_eq!(s.live_cfg, cfg);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
