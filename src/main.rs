//! remember-mcp entrypoint.
//!
//! Runs as a child process of the MCP client (OpenCode): MCP is served over
//! stdio, so the server starts when OpenCode starts and exits when OpenCode
//! closes stdin — no daemon, no background service.
//!
//! Flags:
//!   --check      print install diagnostics as JSON and exit (no MCP)
//!   --version    print version and exit

use remember_mcp::config::Config;
use remember_mcp::server::{spawn_initial_sync, AppState, RememberServer};
use rmcp::ServiceExt;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().filter_or("LOG_LEVEL", "INFO"))
        .target(env_logger::Target::Stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("remember-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--check") {
        return run_check();
    }

    let cfg = Config::from_env();
    let state = AppState::new(cfg)?;
    spawn_initial_sync(state.clone());

    let server = RememberServer::new(state);
    log::info!(
        "remember-mcp v{} serving MCP over stdio (child process; exits with the client)",
        env!("CARGO_PKG_VERSION")
    );

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
            anyhow::Ok(())
        })
}

/// Install verification for humans and AI agents: opens the sources and the
/// index read-only, prints a JSON report, exits 0 when usable.
fn run_check() -> anyhow::Result<()> {
    remember_mcp::register_sqlite_vec();
    let cfg = Config::from_env();
    let index = remember_mcp::index::Index::open(&cfg.index_path)?;

    let db_exists = cfg.opencode_db.exists();
    let storage_exists = cfg.storage_dir.exists();
    let (sessions, chunks) = (index.session_count()?, index.chunk_count()?);
    let adapters = remember_mcp::sources::build_adapters(&cfg.opencode_db, &cfg.storage_dir);
    let mut adapter_report = serde_json::Map::new();
    for a in &adapters {
        let listed = a.list_sessions().map(|v| v.len()).unwrap_or(0);
        adapter_report.insert(
            a.name().to_string(),
            serde_json::json!({"sessions": listed, "skipped_rows": a.skipped()}),
        );
    }

    let report = serde_json::json!({
        "ok": db_exists,
        "version": env!("CARGO_PKG_VERSION"),
        "opencode_db": cfg.opencode_db.display().to_string(),
        "opencode_db_exists": db_exists,
        "storage_dir": cfg.storage_dir.display().to_string(),
        "storage_dir_exists": storage_exists,
        "index_path": cfg.index_path.display().to_string(),
        "index_sessions": sessions,
        "index_chunks": chunks,
        "embed_model": cfg.embed_model,
        "sync_ttl_seconds": cfg.sync_ttl.as_secs(),
        "include_reasoning": cfg.include_reasoning,
        "embed_max_len": cfg.embed_max_len,
        "adapters": adapter_report,
        "hint": if db_exists {
            "sources reachable; run `remember-mcp` (no args) to serve MCP over stdio, or add it to your MCP client config"
        } else {
            "OPENCODE_DB_PATH not found — set it or install OpenCode first"
        },
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    if db_exists {
        Ok(())
    } else {
        std::process::exit(1)
    }
}
