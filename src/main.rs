use std::time::Duration;

use gameboxd_backend::{
    AppState, auth,
    config::Config,
    db::Database,
    igdb::{Endpoints, IgdbClient},
    refresh::Refresher,
    router,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("gameboxd_backend=info,doorman=info,tower_http=info")
        }))
        .init();
    let config = Config::from_env()?;
    let database = Database::connect(&config.database_url).await?;
    let auth = auth::build(&config, &database.auth, database.app.clone())?;
    let igdb = IgdbClient::new(config.igdb_credentials.clone(), Endpoints::default())?;
    if config.igdb_credentials.is_none() {
        tracing::info!("IGDB not configured; local catalog remains available");
    }
    let (refresher, refresh) = Refresher::spawn(database.app.clone(), igdb.clone());
    let app = router(
        &config,
        AppState {
            pool: database.app.clone(),
            igdb,
            refresher,
        },
        &auth,
    );
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(address = %listener.local_addr()?, "Backend listening");

    let moderation_pool = database.app.clone();
    let purge = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = gameboxd_backend::db::purge_moderation(&moderation_pool).await {
                tracing::error!(%error, "Moderation evidence cleanup failed");
            }
            if let Err(error) = gameboxd_backend::db::purge_request_receipts(&moderation_pool).await
            {
                tracing::error!(%error, "Request receipt cleanup failed");
            }
            if let Err(error) = gameboxd_backend::db::purge_auth_audit(&moderation_pool).await {
                tracing::error!(%error, "Authentication audit cleanup failed");
            }
            if let Err(error) = auth.purge_expired(chrono::Utc::now()).await {
                tracing::error!(%error, "Expired authentication data cleanup failed");
            }
            tracing::info!("Maintenance cycle completed");
        }
    });
    // Doorman requires this socket metadata for rate limits and recorded client IPs.
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await;
    purge.abort();
    let _ = purge.await;
    // Serving has returned, so the router state — and with it the only handle
    // on the refresh queue — is gone and the worker stops after at most one
    // already-started refresh. Bound the wait so a slow provider cannot hold
    // shutdown open.
    if tokio::time::timeout(Duration::from_secs(5), refresh)
        .await
        .is_err()
    {
        tracing::warn!("Background metadata refresh did not finish before shutdown");
    }
    database.app.close().await;
    database.auth.pool().close().await;
    result?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install termination handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = terminate => {} }
    tracing::info!("Shutting down; draining requests");
}
