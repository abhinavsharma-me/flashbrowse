//! `flashbrowse` CLI binary.
//!
//! ```text
//! flashbrowse mcp                                   # MCP stdio server
//! flashbrowse open https://github.com                 # ephemeral tree
//! flashbrowse open https://github.com --session-id s1 # persistent tab
//! flashbrowse act 3 --session-id s1 --action click    # click [3] on s1
//! flashbrowse dump https://example.com --format markdown
//! flashbrowse dump https://example.com --format raw   # uncompressed AX tree
//! flashbrowse bench https://example.com
//! flashbrowse setup                                  # managed Chrome
//! flashbrowse close --session-id s1
//! ```

mod commands;
mod resource_policy;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Parse `FLASHBROWSE_TEXT_ONLY` truthy values (`1|true|yes|on`).
fn env_text_only() -> bool {
    std::env::var("FLASHBROWSE_TEXT_ONLY")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[derive(Debug, Parser)]
#[command(name = "flashbrowse", version, about)]
struct Cli {
    /// Explicit Chromium path (overrides `CHROME_PATH`/`CHROME_BINARY`).
    #[arg(long, global = true, env = "CHROME_PATH")]
    chrome_path: Option<PathBuf>,

    /// Block decorative media in addition to fonts/trackers.
    /// Env `FLASHBROWSE_TEXT_ONLY=1|true|yes` enables it; `0|false|no` disables
    /// (bare clap bool-env would treat "false" as true - parsed manually).
    #[arg(long, global = true)]
    text_only: bool,

    /// Load images and fonts (trackers still blocked) for faithful screenshots.
    #[arg(long, global = true, conflicts_with = "text_only")]
    full_render: bool,

    /// Resource load policy: `agent-default`, `text-only`, or `full-render`.
    /// Env `FLASHBROWSE_RESOURCE_POLICY` is used when this flag is absent.
    #[arg(long, global = true)]
    resource_policy: Option<String>,

    /// Enable debug logging to stderr.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Log format on stderr: `text` (default) or `json` (one object per line).
    #[arg(long, global = true, default_value = "text", value_parser = ["text", "json"])]
    log_format: String,

    #[command(subcommand)]
    command: Commands,
}

/// CLI subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the Model Context Protocol stdio server for AI agents.
    Mcp,
    /// Open a URL and display the token-compressed action tree.
    Open {
        /// Target URL. The scheme may be omitted: https is assumed, and local
        /// hosts (localhost, 127.0.0.1, LAN addresses) use http.
        url: String,
        /// Keep the browser alive for `act` (session id).
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Execute a direct action against a persistent tab session.
    Act {
        /// Action id, e.g. `3` for `[3]`.
        action_id: u32,
        /// Persistent session id (from `open --session-id`).
        #[arg(long)]
        session_id: String,
        /// Action to perform.
        #[arg(long, default_value = "click")]
        action: String,
        /// Text payload for `fill`, key for `press_key`, direction for `scroll`.
        #[arg(long)]
        value: Option<String>,
    },
    /// Extract Markdown, the compressed AXTree, or the raw CDP AX tree.
    Dump {
        /// Target URL. The scheme may be omitted: https is assumed, and local
        /// hosts (localhost, 127.0.0.1, LAN addresses) use http.
        url: String,
        /// Output format: `markdown`, `axtree` (compressed flat list),
        /// `axtree-nested` (compressed, indented by depth), `raw`, or
        /// `fidelity` (raw and rendered snapshots from one open)
        /// (uncompressed CDP AX tree as JSON).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Skip compressor pruning (eval arm C/D): every non-ignored AX node.
        #[arg(long)]
        no_prune: bool,
    },
    /// Run token and latency benchmarks against targets.
    Bench {
        /// URLs to benchmark (defaults to `https://example.com`). The scheme
        /// may be omitted: https is assumed, and local hosts use http.
        urls: Vec<String>,
    },
    /// Download managed Chrome-for-Testing (no manual install needed).
    Setup {
        /// Re-download even if already installed.
        #[arg(long)]
        force: bool,
    },
    /// Diagnose the environment (Chrome, cache, disk, memory, sessions).
    Doctor {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Show aggregated metrics (latency, tokens, error rate).
    Stats {
        /// How many recent operations to include.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Update flashbrowse to the latest release (respects FLASHBROWSE_UPDATE_CHANNEL).
    Update {
        /// Only check if a newer release is available without installing.
        #[arg(long)]
        check: bool,
    },
    /// Stop a persistent `--session-id` browser.
    Close {
        /// Persistent session id.
        #[arg(long)]
        session_id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let text_only = cli.text_only || env_text_only();
    let resource_policy = resource_policy::resolve_resource_policy(
        text_only,
        cli.full_render,
        cli.resource_policy.as_deref(),
        std::env::var("FLASHBROWSE_RESOURCE_POLICY").ok().as_deref(),
    )?;
    let log_format = cli
        .log_format
        .parse::<flashbrowse_core::observability::LogFormat>()
        .map_err(|e| anyhow::anyhow!(e))?;

    // Stderr-only logging; stdout stays pure (critical for `mcp`).
    // With `--log-format json` every tracing line is a JSON object too, so a
    // log shipper sees one consistent format.
    let level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    match log_format {
        flashbrowse_core::observability::LogFormat::Json => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_max_level(level)
                .json()
                .flatten_event(true)
                .try_init()
                .ok();
        }
        flashbrowse_core::observability::LogFormat::Text => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_max_level(level)
                .try_init()
                .ok();
        }
    }

    // Graceful shutdown: SIGINT/SIGTERM must not orphan a browser. (On Linux
    // the child also gets PR_SET_PDEATHSIG, so even SIGKILL is covered.)
    //
    // `tokio::signal::unix` only exists on unix; Windows has no SIGTERM, so
    // console Ctrl+C (`ctrl_c`) is the only shutdown signal it can receive.
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = tokio::signal::ctrl_c().await;
                        return;
                    }
                };
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };

    let op_name = command_op_name(&cli.command);

    let run = async {
        match cli.command {
            Commands::Mcp => commands::mcp::run(cli.chrome_path, resource_policy).await,
            Commands::Open { url, session_id } => {
                commands::open::run(url, session_id, cli.chrome_path, resource_policy).await
            }
            Commands::Act {
                action_id,
                session_id,
                action,
                value,
            } => commands::act::run(session_id, action_id, action, value, cli.chrome_path).await,
            Commands::Dump {
                url,
                format,
                no_prune,
            } => commands::dump::run(url, format, no_prune, cli.chrome_path, resource_policy).await,
            Commands::Bench { urls } => {
                commands::bench::run(urls, cli.chrome_path, resource_policy).await
            }
            Commands::Setup { force } => commands::setup::run(force).await,
            Commands::Doctor { json } => {
                let ok = commands::doctor::run(json).await;
                if !ok {
                    std::process::exit(1);
                }
                Ok(())
            }
            Commands::Stats { limit, json } => {
                commands::stats::run(limit, json);
                Ok(())
            }
            Commands::Update { check } => commands::update::run(check).await,
            Commands::Close { session_id } => commands::close::run(session_id).await,
        }
    };

    let started = std::time::Instant::now();
    flashbrowse_core::observability::log_event(
        log_format,
        "info",
        "start",
        &[
            ("op", op_name.clone()),
            ("pid", std::process::id().to_string()),
        ],
    );

    let outcome = tokio::select! {
        result = run => result,
        _ = shutdown => {
            tracing::warn!("interrupted - shutting down");
            flashbrowse_core::observability::log_event(
                log_format,
                "warn",
                "interrupted",
                &[("op", op_name.clone())],
            );
            Ok(())
        }
    };

    let ms = started.elapsed().as_millis() as u64;
    match &outcome {
        Ok(()) => flashbrowse_core::observability::log_event(
            log_format,
            "info",
            "done",
            &[("op", op_name.clone()), ("ms", ms.to_string())],
        ),
        Err(e) => flashbrowse_core::observability::log_event(
            log_format,
            "error",
            "failed",
            &[
                ("op", op_name.clone()),
                ("ms", ms.to_string()),
                ("error", e.to_string()),
            ],
        ),
    }

    outcome
}

/// Stable operation name for logs and metrics.
fn command_op_name(cmd: &Commands) -> String {
    match cmd {
        Commands::Mcp => "mcp".into(),
        Commands::Open { .. } => "open".into(),
        Commands::Act { action, .. } => format!("act:{action}"),
        Commands::Dump { format, .. } => format!("dump:{format}"),
        Commands::Bench { .. } => "bench".into(),
        Commands::Setup { .. } => "setup".into(),
        Commands::Doctor { .. } => "doctor".into(),
        Commands::Stats { .. } => "stats".into(),
        Commands::Update { .. } => "update".into(),
        Commands::Close { .. } => "close".into(),
    }
}
