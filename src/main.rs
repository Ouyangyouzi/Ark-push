mod actors;
mod auth;
mod backup;
mod config;
mod fileindex;
mod indexer;
mod jobs;
mod litepan;
mod organize;
mod pending;
mod search;
mod search_fts;
mod server;
mod state;
mod util;

use clap::{Parser, Subcommand};
use config::{setup_logging, Config};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "ark-push", version, about = "Ark-push: LitePan sidecar without Meilisearch")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the HTTP service
    Serve,
    /// One-shot index sync
    Sync {
        #[arg(long)]
        full: bool,
    },
    /// Trigger LitePan STRM tasks
    Link {
        #[arg(long)]
        dry_run: bool,
    },
    /// Health check
    Doctor,
    /// Print LitePan probe data
    Probe,
    /// Submit a magnet/URL from CLI
    Push {
        #[arg(long)]
        magnet: String,
        #[arg(long, default_value = "")]
        target: String,
        #[arg(long, default_value = "")]
        file_name: String,
        #[arg(long, default_value = "")]
        username: String,
        #[arg(long, default_value = "")]
        password: String,
    },
    /// Plugin management (compatibility placeholder)
    Plugins {
        #[arg(default_value = "list")]
        action: String,
        #[arg(default_value = "")]
        plugin_id: String,
    },
}

async fn build_state(cfg: Config) -> anyhow::Result<Arc<server::AppState>> {
    let state = Arc::new(state::StateStore::new(&cfg.state_dir));
    let auth = Arc::new(auth::AuthStore::new(state.clone()));
    let pool = Arc::new(litepan::LitePanPool::new(cfg.clone(), state.clone()));
    let search = Arc::new(search_fts::SearchStore::new(&cfg.state_dir));
    let fileindex = Arc::new(fileindex::FileIndex::new(&cfg.state_dir, pool.clone()));
    let pending = Arc::new(pending::PendingStore::new(&cfg.state_dir)?);
    let jobs = Arc::new(jobs::JobStore::new());
    let actors = Arc::new(actors::ActorService::new(
        state.clone(),
        search.clone(),
        &cfg.avatar_dir_resolved(),
    ));
    Ok(Arc::new(server::AppState {
        cfg,
        state,
        auth,
        pool,
        search,
        fileindex,
        dir_cache: Arc::new(server::DirCache::default()),
        pending,
        jobs,
        actors,
        started_at: config::iso_now(),
    }))
}

async fn cmd_serve(cfg: Config) -> anyhow::Result<()> {
    let st = build_state(cfg.clone()).await?;
    server::resume_auto_pending(&st);
    let app = server::build_router(st.clone());
    let addr = format!("0.0.0.0:{}", cfg.http_port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Ark-push listening on http://{addr} (embedded search, no Meilisearch)");

    let scheduler_state = st.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
        let mut last_link = std::time::Instant::now();
        loop {
            tick.tick().await;
            if scheduler_state.cfg.link_enabled
                && last_link.elapsed().as_secs() >= scheduler_state.cfg.link_interval_min * 60
            {
                last_link = std::time::Instant::now();
                if let Ok(client) = scheduler_state.pool.client_for(&Value::Null).await {
                    let _ = client.strm_tasks().await;
                }
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await?;
    Ok(())
}

async fn cmd_sync(cfg: Config, full: bool) -> anyhow::Result<()> {
    let st = build_state(cfg).await?;
    let roots = indexer::infer_roots_from_settings(&st.state);
    let report = if !roots.is_empty() {
        let client = st.pool.client_for(&Value::Null).await?;
        indexer::run_netdisk_sync(&client, &roots, &st.search, &st.state, indexer::netdisk_max_dirs()).await
    } else if indexer::has_fileindex(&st.cfg.state_dir) {
        indexer::sync_from_fileindex(&st.cfg.state_dir, &st.search)
    } else {
        indexer::run_strm_sync(&st.cfg.strm_root, &st.search, &st.state, full).await
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn cmd_link(cfg: Config, dry_run: bool) -> anyhow::Result<()> {
    let st = build_state(cfg).await?;
    let client = st.pool.client_for(&Value::Null).await?;
    let tasks = client.strm_tasks().await?;
    let mut triggered = Vec::new();
    for task in tasks {
        let tid = task.get("id").or_else(|| task.get("task_id")).and_then(Value::as_i64).unwrap_or(0);
        if tid == 0 {
            continue;
        }
        if dry_run {
            triggered.push(json!({"id": tid, "name": task.get("name").cloned().unwrap_or(json!("")), "mode": "dry-run"}));
        } else {
            match client.run_strm_task(tid).await {
                Ok(_) => triggered.push(json!({"id": tid, "name": task.get("name").cloned().unwrap_or(json!("")), "mode": "auto"})),
                Err(e) => triggered.push(json!({"id": tid, "error": e.to_string()})),
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&json!({"triggered": triggered}))?);
    Ok(())
}

async fn cmd_doctor(cfg: Config) -> anyhow::Result<i32> {
    let st = build_state(cfg.clone()).await?;
    println!("{}", "=".repeat(62));
    println!("Ark-push doctor");
    println!("{}", "=".repeat(62));
    let root = std::path::Path::new(&cfg.strm_root);
    if !root.is_dir() {
        println!("[FAIL] STRM root not visible: {}", cfg.strm_root);
    } else {
        println!("[ OK ] STRM root visible: {}", cfg.strm_root);
    }
    println!("[ OK ] embedded search docs: {}", st.search.count());
    println!("[ OK ] state dir: {}", cfg.state_dir);
    match st.pool.client_for(&Value::Null).await {
        Ok(client) => match client.login(false).await {
            Ok(_) => {
                let account = client.pick_account(None).await;
                match account {
                    Ok(a) => println!("[ OK ] LitePan login ok, account {}:{}", a.get("id").and_then(Value::as_i64).unwrap_or(0), a.get("name").and_then(Value::as_str).unwrap_or("")),
                    Err(e) => println!("[FAIL] LitePan account: {e}"),
                }
            }
            Err(e) => println!("[FAIL] LitePan login: {e}"),
        },
        Err(e) => println!("[FAIL] LitePan client: {e}"),
    }
    Ok(0)
}

async fn cmd_probe(cfg: Config) -> anyhow::Result<()> {
    let st = build_state(cfg).await?;
    let client = st.pool.client_for(&Value::Null).await?;
    println!("{}", serde_json::to_string_pretty(&client.probe().await)?);
    Ok(())
}

async fn cmd_push(cfg: Config, magnet: String, target: String, file_name: String, username: String, password: String) -> anyhow::Result<i32> {
    let st = build_state(cfg).await?;
    let payload = json!({
        "urls": [magnet],
        "target_path": target,
        "file_name": file_name,
        "username": username,
        "password": password,
    });
    let job = st.jobs.create(&payload);
    let id = job.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    server::run_push(st.clone(), &id, payload).await;
    let final_job = st.jobs.get(&id).unwrap_or(job);
    println!("{}", serde_json::to_string_pretty(&final_job)?);
    Ok(if final_job.get("state").and_then(Value::as_str) == Some("done") { 0 } else { 1 })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[config error] {e}");
            std::process::exit(2);
        }
    };
    setup_logging(&cfg.log_level);
    match cli.command.unwrap_or(Commands::Serve) {
        Commands::Serve => cmd_serve(cfg).await,
        Commands::Sync { full } => cmd_sync(cfg, full).await,
        Commands::Link { dry_run } => cmd_link(cfg, dry_run).await,
        Commands::Doctor => {
            let code = cmd_doctor(cfg).await?;
            if code != 0 { std::process::exit(code); }
            Ok(())
        }
        Commands::Probe => cmd_probe(cfg).await,
        Commands::Push { magnet, target, file_name, username, password } => {
            let code = cmd_push(cfg, magnet, target, file_name, username, password).await?;
            if code != 0 { std::process::exit(code); }
            Ok(())
        }
        Commands::Plugins { action, .. } => {
            if action == "list" {
                println!("(plugins are not enabled in this Rust build)");
            } else {
                eprintln!("plugins are not enabled in this Rust build");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
