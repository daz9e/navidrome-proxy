//! Command line entry point: `navidrome-proxy [serve]` and maintenance commands.

use clap::{Parser, Subcommand};
use navidrome_proxy::audio::Tools;
use navidrome_proxy::config::Settings;
use navidrome_proxy::db::{OwnerLock, Store};
use navidrome_proxy::identity::Identity;
use navidrome_proxy::{app, covers, deezer, imports, migrate};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "navidrome-proxy", version, about = "OpenSubsonic proxy that adds an external catalog to Navidrome")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy (default)
    Serve,
    /// Adopt the first proxy's files and state.json
    MigrateLegacy {
        /// Legacy state.json
        #[arg(long)]
        state: Option<PathBuf>,
        /// Music path used inside the old container
        #[arg(long, default_value = "/music")]
        legacy_music_root: String,
    },
    /// Move files into the Artist/Album layout
    Relayout {
        #[arg(long)]
        dry_run: bool,
    },
    /// Full integrity check of the state database
    CheckDb,
    /// Losslessly fix DASH-fragmented m4a files
    RemuxDash {
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-embed truncated or missing artwork
    RepairCovers {
        #[arg(long)]
        dry_run: bool,
    },
    /// Tag the first proxy's yt-<deezer id>-yt files from the catalog
    AdoptYoutubeFallbacks {
        #[arg(long)]
        dry_run: bool,
    },
    /// Match again import lines whose match credits none of the artists
    RecheckImport {
        import_id: String,
        #[arg(long)]
        dry_run: bool,
    },
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("navidrome-proxy: {message}");
    std::process::exit(1)
}

fn configure_logging(level: &str) {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|f| tracing_subscriber::EnvFilter::try_new(f).ok())
        .unwrap_or_else(|| {
            let level = match level {
                "CRITICAL" => "error",
                "WARNING" => "warn",
                other => other,
            };
            tracing_subscriber::EnvFilter::try_new(format!("{},hyper=warn,reqwest=warn,lofty=warn", level.to_lowercase()))
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        });
    tracing_subscriber::fmt().with_env_filter(filter).with_target(true).with_writer(std::io::stdout).init();
}

fn print(result: Value) {
    println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let settings = Settings::from_process_env();
    configure_logging(&settings.log_level);
    let tools = Tools { ffmpeg: settings.ffmpeg_path.clone(), ffprobe: settings.ffprobe_path.clone(), fpcalc: settings.fpcalc_path.clone() };
    let command = cli.command.unwrap_or(Command::Serve);
    if let Command::RemuxDash { dry_run } = command {
        // Files only: safe next to a running server.
        print(Value::Object(migrate::remux_dash(&tools, &settings.music_dir, dry_run).await));
        return;
    }
    let mut owner = OwnerLock::new(&settings.db_path());
    if let Err(e) = owner.acquire() {
        fail(e);
    }
    if let Command::Serve = command {
        if let Err(e) = app::serve(settings).await {
            fail(format!("{e:#}"));
        }
        return;
    }
    let store = Arc::new(Store::open(&settings.db_path()).unwrap_or_else(|e| fail(e)));
    let covers = || covers::Covers::new(&settings.cache_dir().join("covers"));
    let result = match command {
        Command::MigrateLegacy { state, legacy_music_root } => {
            Value::Object(migrate::migrate_legacy(&store, &tools, &settings.music_dir, state.as_deref(), &legacy_music_root).await)
        }
        Command::CheckDb => {
            let store = store.clone();
            let problems = tokio::task::spawn_blocking(move || store.integrity(false)).await.unwrap_or_default();
            json!({"ok": problems.is_empty(), "problems": problems.into_iter().take(20).collect::<Vec<_>>()})
        }
        Command::Relayout { dry_run } => Value::Object(migrate::relayout(&store, &settings.music_dir, dry_run)),
        Command::RecheckImport { import_id, dry_run } => match imports::recheck(&store, &Identity::new(store.clone()), &import_id, dry_run) {
            Some(stats) => Value::Object(stats),
            None => fail(format!("no import {import_id}")),
        },
        Command::RepairCovers { dry_run } => {
            Value::Object(migrate::repair_covers(&store, &tools, &settings.music_dir, &covers(), &settings.staging_dir(), dry_run).await)
        }
        Command::AdoptYoutubeFallbacks { dry_run } => {
            // A low rate leaves room for the running proxy's own Deezer budget.
            let dz = deezer::Deezer::new(store.clone(), 3.0, deezer::API, settings.search_cache_seconds);
            Value::Object(migrate::adopt_youtube_fallbacks(&store, &tools, &settings.music_dir, &dz, &covers(), &settings.staging_dir(), dry_run, 15.0).await)
        }
        Command::Serve | Command::RemuxDash { .. } => unreachable!(),
    };
    owner.release();
    print(result);
}
