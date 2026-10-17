//! Inkult — self-hosted music downloader and library manager.

mod app;
mod auth;
mod config;
mod domain;
mod jobs;
mod library;
mod metadata;
mod playback;
mod providers;
mod rooms;
mod search;
mod settings;
mod soulseek;
mod storage;
mod tools;
mod web;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

use crate::app::App;
use crate::auth::Auth;
use crate::config::Config;
use crate::jobs::{JobManager, PipelineContext, RetryPolicy};
use crate::library::Library;
use crate::playback::Playback;
use crate::providers::{MusicProvider, ProviderRegistry, SoundCloudProvider, YouTubeProvider};
use crate::rooms::Rooms;
use crate::soulseek::{Soulseek, SoulseekConfig};
use crate::tools::ffmpeg::Ffmpeg;
use crate::tools::ytdlp::YtDlp;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("inkult=info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("passwd") => reset_password(&args[1..]),
        Some("unlock") => unlock(&args[1..]),
        Some("reset-2fa") => reset_two_factor(&args[1..]),
        Some(other) => Err(format!(
            "unknown command '{other}'; usage: inkult [passwd <handle> | unlock <handle> | reset-2fa <handle>]"
        )
        .into()),
        None => run().await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    config.prepare_directories()?;

    let ytdlp = Arc::new(YtDlp::new(&config.ytdlp_program, &config.ffmpeg_program));
    let ffmpeg = Arc::new(Ffmpeg::new(&config.ffmpeg_program));
    let library = Arc::new(Library::open(config.library_dir.clone(), config.index_path()).await?);
    let settings = open_settings(&config).await?;
    let server_settings = settings.get().await;
    let auth = Auth::open(config.users_path(), server_settings.auth()).await?;
    let rooms = Rooms::open(config.rooms_dir()).await?;

    let providers: Vec<Arc<dyn MusicProvider>> = vec![
        Arc::new(YouTubeProvider::new(Arc::clone(&ytdlp))),
        Arc::new(SoundCloudProvider::new(Arc::clone(&ytdlp))),
    ];
    let retry = RetryPolicy {
        max_retries: config.max_retries,
        base_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(60),
    };
    let jobs = JobManager::new(
        PipelineContext {
            library: Arc::clone(&library),
            ffmpeg: Arc::clone(&ffmpeg),
            staging_dir: config.staging_dir.clone(),
            retry,
            max_track_length: settings.track_limit(),
        },
        config.max_concurrent_downloads,
        config.max_queued_jobs,
    );

    let playback = Arc::new(
        Playback::new(Arc::clone(&ffmpeg))
            .map_err(|e| format!("cannot initialise the HTTP client for streaming: {e}"))?,
    );

    let soulseek = Arc::new(
        Soulseek::new(
            SoulseekConfig {
                server: config.soulseek_server.clone(),
                ports: config.soulseek_ports.clone(),
                upload_slots: config.soulseek_upload_slots,
                max_track_length: settings.track_limit(),
                staging_dir: config.staging_dir.clone(),
                store_path: config.soulseek_path(),
                allow_loopback: false,
            },
            Arc::clone(&library),
        )
        .await?,
    );

    let bind = config.bind;
    let app = Arc::new(
        App::new(
            config.clone(),
            settings,
            auth,
            ProviderRegistry::new(providers),
            jobs.clone(),
            library,
            playback,
            Arc::clone(&soulseek),
            rooms,
            ytdlp,
            ffmpeg,
        )
        .await,
    );

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("cannot listen on {bind}: {e}"))?;
    tracing::info!(
        library = %config.library_dir.display(),
        "Inkult listening on http://{}",
        listener.local_addr()?
    );
    let clients = Arc::new(web::ClientPolicy::new(config.trusted_proxies.clone()));
    web::serve(
        listener,
        web::router(app, Arc::clone(&clients)),
        clients,
        shutdown_signal(),
    )
    .await;

    tracing::info!("shutting down; cancelling running downloads");
    jobs.cancel_all();
    soulseek.shutdown().await;
    // Give job tasks a moment to kill child processes and clean staging.
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Load the admin-panel settings, creating them on the first start, and
/// say which environment variables no longer have an effect.
async fn open_settings(config: &Config) -> Result<settings::Settings, Box<dyn std::error::Error>> {
    let path = config.settings_path();
    let (settings, created) = settings::Settings::open(path.clone(), &config.initial_settings)
        .await
        .map_err(|e| e.to_string())?;
    let current = settings.get().await;
    if created {
        tracing::info!(path = %path.display(), two_factor = ?current.two_factor, "server settings created; captains change them in the ADMIN view");
    } else {
        for var in &config.initial_settings_vars {
            tracing::warn!(
                "{var} is ignored: it only seeds the first start; change it in the ADMIN view"
            );
        }
    }
    for var in &config.retired_vars {
        tracing::warn!(two_factor = ?current.two_factor, "{var} is no longer read; who must use two-factor login is set in the ADMIN view");
    }
    Ok(settings)
}

/// `inkult passwd <handle>`: set an account's password, reading it from
/// standard input (one line). For when the only captain is locked out.
fn reset_password(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [handle] = args else {
        return Err("usage: inkult passwd <handle>  (new password on standard input)".into());
    };
    let config = Config::from_env()?;
    eprintln!("Stop Inkult before running this, or the running server will overwrite the change.");
    eprintln!("New password for '{handle}' (one line):");
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']);
    auth::reset_password_offline(config.users_path(), handle, password)?;
    eprintln!("Password changed.");
    Ok(())
}

/// `inkult unlock <handle>`: lift a lock set by too many failed logins,
/// for when no captain who could do it from the web UI can log in.
fn unlock(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [handle] = args else {
        return Err("usage: inkult unlock <handle>".into());
    };
    let config = Config::from_env()?;
    eprintln!("Stop Inkult before running this, or the running server will overwrite the change.");
    auth::unlock_offline(config.users_path(), handle)?;
    eprintln!("Account '{handle}' unlocked.");
    Ok(())
}

/// `inkult reset-2fa <handle>`: remove two-factor login from an account
/// whose authenticator and recovery codes are lost, when no other captain
/// can reset it from the web UI.
fn reset_two_factor(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [handle] = args else {
        return Err("usage: inkult reset-2fa <handle>".into());
    };
    let config = Config::from_env()?;
    eprintln!("Stop Inkult before running this, or the running server will overwrite the change.");
    auth::reset_two_factor_offline(config.users_path(), handle)?;
    eprintln!("Two-factor login removed from '{handle}'. Their password is unchanged.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
