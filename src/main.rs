use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};

use claw::config::Config;
use claw::logging;

#[derive(Parser)]
#[command(name = "claw", version, about = "Personal AI assistant daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the claw daemon
    Serve,
    /// Admin command sent to a running daemon, e.g. `claw groups list`
    #[command(external_subcommand)]
    Admin(Vec<String>),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => serve(),
        Command::Admin(argv) => admin(&argv),
    }
}

fn admin(argv: &[String]) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let socket_path = config.socket_path();
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(claw::cli_client::run(&socket_path, argv));
    if let Err(error) = outcome {
        eprintln!("{error}");
        std::process::exit(1);
    }
    Ok(())
}

fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let _log_guard = logging::init(&config.logs_dir())?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(config))
}

async fn run(config: Config) -> anyhow::Result<()> {
    tracing::info!(
        data_dir = %config.data_dir.display(),
        port = config.port,
        "claw starting"
    );
    let circuit_breaker = claw::circuit_breaker::CircuitBreaker::new(&config.data_dir);
    let backoff = circuit_breaker.record_start(jiff::Timestamp::now());
    if !backoff.is_zero() {
        tracing::warn!(
            seconds = backoff.as_secs(),
            "crash-loop detected — backing off before starting"
        );
        tokio::time::sleep(backoff).await;
    }

    let app = claw::app::build(&config).await?;
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, config.port)).await?;
    tracing::info!(port = config.port, "web interface listening");

    let http = app.http.clone();
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, http).await {
            tracing::error!(%error, "http server stopped");
        }
    });

    wait_for_shutdown_signal().await?;
    tracing::info!("shutdown signal received, exiting");
    server.abort();
    app.shutdown().await;
    circuit_breaker.clear(); // clean exit — next start resets the crash counter
    Ok(())
}

async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}
