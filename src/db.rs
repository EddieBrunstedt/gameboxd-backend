use std::time::Duration;

use doorman::prelude::PostgresStorage;
use sqlx::{PgPool, postgres::PgPoolOptions};

pub struct Database {
    pub app: PgPool,
    pub auth: PostgresStorage,
}

impl Database {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let bootstrap = PgPoolOptions::new().max_connections(1).connect(url).await?;
        sqlx::raw_sql(
            "CREATE SCHEMA IF NOT EXISTS gameboxd_auth; CREATE SCHEMA IF NOT EXISTS gameboxd;",
        )
        .execute(&bootstrap)
        .await?;
        bootstrap.close().await;

        // Each schema owns its own _sqlx_migrations table. Never run the app's
        // migrations in Doorman's schema (their version numbers may overlap).
        let auth = PostgresStorage::from_pool(pool(url, "SET search_path TO gameboxd_auth").await?);
        auth.migrate().await?;
        let app = pool(url, "SET search_path TO gameboxd").await?;
        sqlx::migrate!("./migrations").run(&app).await?;
        Ok(Self { app, auth })
    }
}

async fn pool(url: &str, search_path: &'static str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            Box::pin(async move {
                sqlx::query(search_path).execute(connection).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}

/// Scrub private moderation evidence after 90 days; retain only minimal case
/// metadata for deduplication and action history. Reads enforce the same cutoff.
pub async fn purge_moderation(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE moderation_reports SET evidence=NULL,details='' WHERE created_at<=now()-interval '90 days' AND (evidence IS NOT NULL OR details<>'')")
        .execute(&mut *tx).await?;
    sqlx::query("UPDATE moderation_audit SET reason='Expired' WHERE created_at<=now()-interval '90 days' AND reason<>'Expired'")
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Expired receipts retain only a fingerprint/key tombstone to prevent late
/// retries from duplicating history. No request text is stored in tombstones.
pub async fn purge_request_receipts(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE request_receipts SET body=NULL,etag=NULL WHERE expires_at<=clock_timestamp() AND body IS NOT NULL")
        .execute(pool).await?;
    Ok(())
}
