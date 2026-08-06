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

/// The frontend-development override for the compiled-in bundle.
const STATIC_DIR_ENV: &str = "STAX_STATIC_DIR";

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

    /// The `stackunderflow/` package directory — `deps.BASE_DIR`.
    /// `data/models.toml` and `infra/model_candidates.json` are read from it
    /// when it exists, and fall back to the copies compiled into the binary
    /// when it does not.
    #[arg(long)]
    package_dir: Option<PathBuf>,

    /// Serve `/static`, `/assets`, `/favicon.ico` and the SPA from this
    /// directory instead of from the bundle compiled into the binary.
    ///
    /// The frontend-development override, and the only way to reach the disk
    /// for a static file: point it at a `vite build` output and every static
    /// read goes back through `tower_http`'s `ServeDir`, exactly as it did
    /// before the bundle was embedded. `$STAX_STATIC_DIR` sets the same thing;
    /// the flag wins.
    ///
    /// (The variable is read in `main`, not by `clap`'s `env` feature — taking
    /// that feature would be a workspace-wide manifest change for one string.)
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Print the resolved paths and exit without binding.
    #[arg(long)]
    check: bool,

    /// Permit binding 8095 — the resident port.
    ///
    /// The flip (2026-08-05): the maintainer's resident server IS this binary,
    /// which retires the reservation's premise — but only for a supervised,
    /// deliberate boot. `stax start` passes this when the resolved port is
    /// 8095; every unsupervised invocation (harnesses spawn this binary raw)
    /// still refuses, so a parity run can never take the resident port by
    /// accident.
    #[arg(long)]
    resident: bool,

    /// Run one static-analysis verb and exit without binding — the DIV-308
    /// spawn surface for `stax analyze {session,backfill,quality}`.
    ///
    /// The value is a JSON request: `{"verb": "session"|"backfill"|"quality",
    /// …verb args…}`. The response is one JSON object on stdout (the CLI
    /// renders text from it); a failure is `{"error": {"kind", "message"}}`
    /// on stdout with exit 1, so the CLI can map `bad_parameter` onto
    /// click's rendering. This lives on THIS binary because the analyzers
    /// need Playback v2 and grading, which `stax-cli` may not link.
    #[arg(long, value_name = "REQUEST_JSON")]
    analyze: Option<String>,

    /// Serve ONLY `/api/webhooks/*` — `cli.py`'s `ingest webhook serve`.
    ///
    /// The reference builds a second, bare `FastAPI()` and includes just the
    /// webhook router, so the receiver can face a tunnel without the dashboard
    /// facing it too. Same shape here, through
    /// [`stax_server::webhook_receiver_app`]. It is a flag on this binary
    /// rather than a binary of its own for the reason `start` spawns this one:
    /// `stax-cli`'s dependency graph deliberately contains no axum.
    #[arg(long)]
    webhooks_only: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    anyhow::ensure!(
        cli.port != 8095 || cli.resident,
        "port 8095 is the resident port — pass --resident to take it deliberately \
         (harness and parity invocations never do; see the flag's doc)"
    );

    let app_dir = cli.data_dir.unwrap_or_else(stax_core::settings::app_dir);
    let store_path = app_dir.join("store.db");
    let package_dir = cli.package_dir.unwrap_or_else(default_package_dir);
    let static_dir = cli.static_dir.or_else(|| {
        std::env::var_os(STATIC_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let config = Config::load(&app_dir);

    if let Some(request) = &cli.analyze {
        let (payload, code) =
            stax_server::services::static_analysis::run_bin_request(request, &store_path);
        println!("{payload}");
        std::process::exit(code);
    }

    if cli.check {
        println!("store    {}", store_path.display());
        println!("package  {}", package_dir.display());
        match &static_dir {
            Some(dir) => println!("static   {} (override)", dir.display()),
            None => println!(
                "static   embedded ({} files, {} bytes)",
                stax_server::assets::len(),
                stax_server::assets::keys()
                    .filter_map(stax_server::assets::get)
                    .map(<[u8]>::len)
                    .sum::<usize>()
            ),
        }
        println!("currency {}", config.currency);
        return Ok(());
    }

    anyhow::ensure!(
        store_path.is_file(),
        "no store at {} — point --data-dir at a StackUnderflow home",
        store_path.display()
    );

    let mut state = AppState::with_static_dir(store_path, package_dir, config, static_dir);
    if cli.resident {
        state = state.into_resident();
    }
    let app = if cli.webhooks_only {
        stax_server::webhook_receiver_app(state)
    } else {
        stax_server::app(state)
    };

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
/// Still the compile-time repo layout, which is what every campaign invocation
/// wants and what keeps the parity harness reading one shared tree. It is no
/// longer load-bearing: as of wave-10 item 2c the static bundle is compiled in,
/// and `data/models.toml` / `infra/model_candidates.json` fall back to their
/// compiled-in copies when this path does not exist — so a binary run from a
/// machine with no `stackunderflow/` at all still serves.
fn default_package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets")
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
