#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod gateway;
mod state;
mod store;
#[cfg(any(windows, wincheck))]
mod ui;
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
    }
}

fn main() {
    init_log();
    tracing::info!("tinycord {} starting", env!("CARGO_PKG_VERSION"));
    store::load_settings();

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
