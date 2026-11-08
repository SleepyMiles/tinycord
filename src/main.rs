#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod capture;
mod convert;
mod gateway;
mod golive;
mod state;
mod rtp;
mod store;
mod stream;
mod sysaudio;
#[cfg(any(windows, wincheck))]
mod ui;
#[cfg(any(windows, wincheck))]
mod ui_live;
mod video;
mod voice;

fn init_log() {
    let path = store::dir().join("log.txt");
    if let Ok(file) = std::fs::File::create(path) {
        let filter = tracing_subscriber::EnvFilter::new("warn,tinycord=info,songbird=info,davey=info");
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_env_filter(filter)
            .init();
        // A crash in any thread goes to the log too (there's no console).
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!("crash: {info}");
            default(info);
        }));
    }
}

/// Lists audio devices on a background thread. The first time it sees a
/// Krisp microphone (and no mic was picked before), it selects it and turns
/// off tinycord's own noise suppression so the audio isn't cleaned twice.
pub fn refresh_devices() {
    std::thread::spawn(|| {
        let (ins, outs) = audio::list_devices();
        let mut s = state::state();
        let mut picked_krisp = false;
        if !s.mic_chosen {
            if let Some(k) = ins.iter().find(|n| n.to_lowercase().contains("krisp")) {
                s.mic_device = Some(k.clone());
                s.mic_chosen = true;
                picked_krisp = true;
            }
        }
        s.input_devices = ins;
        s.output_devices = outs;
        drop(s);
        if picked_krisp {
            voice::shared()
                .noise_suppression
                .store(false, std::sync::atomic::Ordering::Relaxed);
            store::save_settings();
            state::set_status("Using Krisp Microphone");
            state::send(state::Cmd::RestartAudio);
        }
        state::changed();
    });
}

fn main() {
    init_log();
    tracing::info!("tinycord {} starting", env!("CARGO_PKG_VERSION"));
    store::load_settings();
    refresh_devices();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    state::set_cmd_sender(tx);

    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio");
            rt.block_on(gateway::run(rx));
        })
        .expect("net thread");

    #[cfg(any(windows, wincheck))]
    {
        std::thread::spawn(|| voice::transmit_loop(ui::key_down));
        ui::run();
    }

    // Non-Windows builds exist only so the network/voice code can be checked
    // on Linux. Log in with TINYCORD_TOKEN and print status.
    #[cfg(not(any(windows, wincheck)))]
    {
        std::thread::spawn(|| voice::transmit_loop(|_| false));
        if let Ok(t) = std::env::var("TINYCORD_TOKEN") {
            state::send(state::Cmd::Login(t));
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            println!("{}", state::state().status);
        }
    }
}
