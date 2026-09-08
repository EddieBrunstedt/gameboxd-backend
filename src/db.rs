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
