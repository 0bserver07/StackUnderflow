//! The dashboard binary — `stackunderflow start`'s uvicorn call, in Rust.
//!
//! Everything the app needs is resolved here and injected; nothing below this
//! file reads the environment. The default port is **8096**, not 8081 and never
//! 8095: `docs/specs/rust-port.md` §5 reserves 8095 for the running Python
//! instance on the maintainer's machine, and the campaign binds 8096.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use stax_server::state::{AppState, Config};

/// The campaign's port. 8095 belongs to the maintainer's Python server and is
/// never bound by anything in this workspace.
const DEFAULT_PORT: u16 = 8096;

#[derive(Parser, Debug)]
#[command(
    name = "stax-server",
    about = "Serve the StackUnderflow dashboard from the Rust port",
    long_about = None,
)]
struct Cli {
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,

    /// Bind port. Never 8095 — that is the Python server's.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Data directory (`$STACKUNDERFLOW_HOME`). Defaults to the same
    /// resolution `settings.app_dir()` performs.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// The `stackunderflow/` package directory — `deps.BASE_DIR`. `static/`
    /// (the React oracle) and `data/models.toml` hang off it.
    #[arg(long)]
    package_dir: Option<PathBuf>,

    /// Print the resolved paths and exit without binding.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    anyhow::ensure!(
        cli.port != 8095,
        "port 8095 is reserved for the Python server (docs/specs/rust-port.md §5)"
    );

    let app_dir = cli.data_dir.unwrap_or_else(stax_core::settings::app_dir);
    let store_path = app_dir.join("store.db");
    let package_dir = cli.package_dir.unwrap_or_else(default_package_dir);
    let config = Config::load(&app_dir);

    if cli.check {
        println!("store    {}", store_path.display());
        println!("package  {}", package_dir.display());
        println!("static   {}", package_dir.join("static").display());
        println!("currency {}", config.currency);
        return Ok(());
    }

    anyhow::ensure!(
        store_path.is_file(),
        "no store at {} — point --data-dir at a StackUnderflow home",
        store_path.display()
    );

    let state = AppState::new(store_path, package_dir, config);
    let app = stax_server::app(state);

    let addr = SocketAddr::new(cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    // The harness waits on this line, so it goes to stdout unbuffered and says
    // the bound address rather than the requested one (port 0 is a legal ask).
    let bound = listener.local_addr().context("reading the bound address")?;
    println!("stax-server listening on http://{bound}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving")?;
    Ok(())
}

/// `deps.BASE_DIR` when the binary runs out of this worktree.
///
/// Falls back to the compile-time repo layout, which is what every campaign
/// invocation wants; a packaged install would pass `--package-dir`.
fn default_package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow")
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
