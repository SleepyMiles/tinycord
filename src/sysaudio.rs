//! Your PC's sound for the stream: everything Windows is playing, except
//! tinycord itself (so viewers don't hear the voice chat twice). Uses
//! WASAPI process loopback, Windows 10 2004 or newer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 20 ms of 48 kHz stereo, interleaved.
pub const CHUNK: usize = 960 * 2;

/// Capture until `stop` is set, handing `sink` 20 ms chunks and how long
/// it's been since the previous chunk (silence isn't delivered at all).
#[cfg(windows)]
pub fn run(stop: Arc<AtomicBool>, mut sink: impl FnMut(&[f32], std::time::Duration)) -> Result<(), String> {
    use std::collections::VecDeque;
    use std::time::Instant;
    use wasapi::*;

    let _ = initialize_mta();
    let format = WaveFormat::new(32, 32, SampleType::Float, 48000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(std::process::id(), false)
        .map_err(|e| format!("PC sound: {e}"))?;
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
    client
        .initialize_client(&format, Direction::Capture, mode, None)
        .map_err(|e| format!("PC sound: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| format!("PC sound: {e}"))?;
    let capture = client.get_audiocaptureclient().map_err(|e| format!("PC sound: {e}"))?;
    client.start_stream().map_err(|e| format!("PC sound: {e}"))?;

    let mut raw: VecDeque<u8> = VecDeque::with_capacity(48000 * 8 / 5);
    let mut samples: Vec<f32> = Vec::with_capacity(CHUNK * 4);
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let _ = event.wait_for_event(100);
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => {}
                Ok(_) => break,
                Err(e) => {
                    let _ = client.stop_stream();
                    return Err(format!("PC sound stopped: {e}"));
                }
            }
            if let Err(e) = capture.read_from_device_to_deque(&mut raw) {
                let _ = client.stop_stream();
                return Err(format!("PC sound stopped: {e}"));
            }
        }
        while raw.len() >= 4 {
            let b = [raw[0], raw[1], raw[2], raw[3]];
            raw.drain(..4);
            samples.push(f32::from_le_bytes(b));
        }
        let mut used = 0;
        while samples.len() - used >= CHUNK {
            let now = Instant::now();
            sink(&samples[used..used + CHUNK], now.duration_since(last));
            last = now;
            used += CHUNK;
        }
        samples.drain(..used);
        // Never let a backlog build up (it would only add delay).
        if samples.len() > CHUNK * 10 {
            let cut = samples.len() - CHUNK * 2;
            samples.drain(..cut);
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

#[cfg(not(windows))]
pub fn run(stop: Arc<AtomicBool>, _sink: impl FnMut(&[f32], std::time::Duration)) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}
