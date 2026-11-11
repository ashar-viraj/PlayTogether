mod audio;
mod clock;
mod controller;
mod net;
mod room;
mod server;
mod sinkctl;
mod spatial;
mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::serve::ListenerExt;
use clap::Parser;
use tokio::sync::broadcast;

use crate::state::{App, AudioCtl};

#[derive(Parser)]
#[command(version, about = "Play this PC's audio on every phone and PC in the room, in sync")]
struct Args {
    /// Port for the receiver page and the dashboard.
    #[arg(long, default_value_t = 8090)]
    port: u16,

    /// Fixed room delay in ms. By default it's automatic: as low as every device allows.
    #[arg(long)]
    delay_ms: Option<f64>,

    /// Start with this PC's speakers muted (receivers only). Can be toggled in the dashboard.
    #[arg(long)]
    mute_pc: bool,

    /// PipeWire sink name this PC should play on. Default: whatever the output is right now.
    #[arg(long)]
    pc_output: Option<String>,

    /// Let receivers on this PC get audio (testing; a browser here would loop audio back).
    #[arg(long, hide = true)]
    dev_allow_local_receivers: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let defaults = sinkctl::read()?;
    if defaults.effective.as_deref() == Some(audio::SINK_NAME) {
        bail!("PlayTogether already seems to be running (its sink is the default output)");
    }
    let pc_output = args
        .pc_output
        .or(defaults.effective)
        .context("no audio output found; pass --pc-output <sink name>")?;
    let restore_to = sinkctl::remember_user_default(defaults.configured);

    let ctl = Arc::new(AudioCtl::new(args.delay_ms.unwrap_or(100.0), args.mute_pc));
    let spatial = Arc::new(spatial::Shared::default());
    let (audio_tx, _) = broadcast::channel(128);
    let audio = audio::start(ctl.clone(), spatial.clone(), audio_tx.clone(), pc_output.clone())?;
    sinkctl::set_configured(Some(audio::SINK_NAME))?;

    let mut app = App::new(ctl, spatial, audio_tx, args.delay_ms.is_none(), pc_output.clone(), net::scan(args.port));
    app.allow_local_receivers = args.dev_allow_local_receivers;
    let app = Arc::new(app);
    app.relayout();
    if let Some(ms) = args.delay_ms {
        app.set_delay_ms(ms);
    }
    tokio::spawn(controller::run(app.clone()));
    tokio::spawn(watch_network(app.clone(), args.port));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port))
        .await
        .with_context(|| format!("can't listen on port {}", args.port))?
        .tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
    let server = axum::serve(
        listener,
        server::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    );

    println!("PlayTogether is live. Everything this PC plays now goes to the whole room.");
    println!("  PC speakers: {pc_output}");
    println!("  Dashboard:   http://localhost:{}/host", args.port);
    print_join_urls(&app.net.lock().unwrap().join_urls);
    println!("Press Ctrl+C to stop and restore your normal audio output.");
    println!("Devices that open the page show up below:");

    tokio::select! {
        r = server.into_future() => r.context("web server stopped")?,
        _ = shutdown_signal() => {}
    }

    println!("\nRestoring your audio output...");
    if let Err(e) = sinkctl::set_configured(restore_to.as_deref()) {
        eprintln!("warning: couldn't restore the default output: {e:#}");
    } else {
        sinkctl::forget_user_default();
    }
    // Give WirePlumber a moment to move apps back before our sink disappears.
    tokio::time::sleep(Duration::from_millis(300)).await;
    audio.stop();
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}

fn print_join_urls(urls: &[String]) {
    match urls {
        [] => println!("  No LAN address found; connect this PC to Wi-Fi/Ethernet so other devices can join."),
        [best, others @ ..] => {
            println!("  Join from phones/PCs: {best}");
            for url in others {
                println!("     (other network): {url}");
            }
        }
    }
}

/// Re-scan our addresses so the QR code and the "is this the host?" check follow Wi-Fi changes.
async fn watch_network(app: Arc<App>, port: u16) {
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        let fresh = net::scan(port);
        let mut net = app.net.lock().unwrap();
        if *net != fresh {
            if net.join_urls != fresh.join_urls {
                println!("Network changed.");
                print_join_urls(&fresh.join_urls);
            }
            *net = fresh;
        }
    }
}
