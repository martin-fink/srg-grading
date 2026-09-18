//! Server-rendered grading portal and narrow executor API.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use grading_core::diagnostics::Stage;
use grading_web::routes::{self, AppState};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:8081")]
    internal_listen: SocketAddr,
    #[arg(long)]
    database_url_file: Option<PathBuf>,
    #[arg(long)]
    github_config: Option<PathBuf>,
    #[arg(long)]
    webhook_secret_file: Option<PathBuf>,
    #[arg(long, default_value = "/var/lib/grading/artifacts")]
    artifact_dir: PathBuf,
    #[arg(long)]
    public_url: Option<String>,
    #[arg(long)]
    preview: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    for directive in [
        "reqwest=off",
        "hyper=off",
        "hyper_util=off",
        "sqlx=off",
        "tower_http=off",
    ] {
        filter = filter.add_directive(directive.parse().unwrap());
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    match run(Args::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let details = grading_store::diagnostics(&error);
            tracing::error!(
                stage = details.stage,
                reason = details.reason,
                upstream_status = details.upstream_status,
                "web service terminated"
            );
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<()> {
    if args.preview {
        ensure!(
            args.listen.ip().is_loopback(),
            "preview must bind to loopback"
        );
        tracing::info!(address=%args.listen,"starting design preview");
        axum::serve(
            tokio::net::TcpListener::bind(args.listen)
                .await
                .context(Stage("public_listener_bind"))?,
            routes::preview_router(),
        )
        .with_graceful_shutdown(shutdown())
        .await?;
        return Ok(());
    }
    let pool = grading_store::connect(
        &args
            .database_url_file
            .context("--database-url-file is required")?,
    )
    .await?;
    let github = grading_github::GitHub::from_file(
        &args.github_config.context("--github-config is required")?,
    )
    .await
    .context(Stage("github_configuration"))?;
    let secret = tokio::fs::read(
        args.webhook_secret_file
            .context("--webhook-secret-file is required")?,
    )
    .await
    .context(Stage("webhook_secret_read"))?;
    ensure!(
        secret.len() >= 32,
        "webhook secret must have at least 32 bytes"
    );
    let public_url = reqwest::Url::parse(&args.public_url.context("--public-url is required")?)?;
    ensure!(
        public_url.scheme() == "https"
            && public_url.path() == "/"
            && public_url.query().is_none()
            && public_url.fragment().is_none(),
        "public URL must be an HTTPS origin"
    );
    let maintenance_pool = pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            if grading_store::identity::cleanup_expired(&maintenance_pool)
                .await
                .is_err()
            {
                tracing::warn!(
                    stage = "session_cleanup",
                    "Expired authentication cleanup failed"
                );
            }
        }
    });
    let state = AppState {
        pool,
        github,
        artifacts: grading_store::artifacts::Artifacts::new(args.artifact_dir).await?,
        webhook_secret: Arc::new(secret),
        public_origin: public_url.origin().ascii_serialization(),
    };
    let public = tokio::net::TcpListener::bind(args.listen)
        .await
        .context(Stage("public_listener_bind"))?;
    let internal = tokio::net::TcpListener::bind(args.internal_listen)
        .await
        .context(Stage("internal_listener_bind"))?;
    tracing::info!(public=%args.listen,internal=%args.internal_listen,"starting grading portal");
    tokio::try_join!(
        async {
            axum::serve(public, routes::public_router(state.clone()))
                .with_graceful_shutdown(shutdown())
                .await
        },
        async {
            axum::serve(internal, routes::internal_router(state.clone()))
                .with_graceful_shutdown(shutdown())
                .await
        }
    )?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("signal handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
