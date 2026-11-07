// Feeds a 440 Hz tone through the same mic path songbird uses and decodes it.
#[path = "../src/audio.rs"]
mod audio;
mod state { pub fn set_status(_: impl Into<String>) {} }
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::{AudioStream, LiveInput, RawAdapter};
fn main() {
    let sh = audio::Shared::new();
    {
        let mut m = sh.mic.lock();
        for i in 0..48000 { m.push_back((i as f32 * 440.0 * 6.283 / 48000.0).sin() * 0.3); }
    }
    let src = RawAdapter::new(audio::MicSource::new(sh.clone()), 48000, 1);
    let live = LiveInput::Raw(AudioStream { input: Box::new(src) as Box<dyn songbird::input::core::io::MediaSource> });
    let p = live.promote(get_codec_registry(), get_probe()).expect("promote");
    let mut parsed = match p { LiveInput::Parsed(p) => p, _ => panic!("not parsed") };
    let mut total = 0usize; let mut peak = 0f32;
    for _ in 0..20 {
        let pkt = parsed.format.next_packet().expect("packet");
        let buf = parsed.decoder.decode(&pkt).expect("decode");
        let mut sb = songbird::input::core::audio::SampleBuffer::<f32>::new(buf.capacity() as u64, *buf.spec());
        sb.copy_interleaved_ref(buf);
        total += sb.samples().len();
        for s in sb.samples() { peak = peak.max(s.abs()); }
    }
    println!("decoded {total} samples, peak {peak:.3}");
}
