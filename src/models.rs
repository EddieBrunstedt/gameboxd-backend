use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, types::Json};
use uuid::Uuid;

use crate::{
    error::ApiResult,
    igdb::{IgdbClient, NamedItem, ProviderGame},
};

#[derive(Debug, Serialize, FromRow)]
pub struct Game {
    pub id: Uuid,
    pub igdb_id: i64,
    pub name: String,
    pub slug: String,
    pub summary: Option<String>,
    pub cover_url: Option<String>,
    pub first_release_date: Option<DateTime<Utc>>,
    pub platforms: Json<Vec<NamedItem>>,
    pub genres: Json<Vec<NamedItem>>,
    pub developers: Json<Vec<NamedItem>>,
    pub game_type: Option<i64>,
    pub parent_igdb_id: Option<i64>,
    pub version_parent_igdb_id: Option<i64>,
    pub source_url: String,
    pub imported_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
}

/// The local row for an upstream game, importing or refreshing it as needed.
///
/// Materializing a game is a side effect of an operation the user actually
/// intends — adding it to a library, recording a playthrough, or an explicit
/// import — never of a search or catalog lookup.
/// If upstream is unavailable but a stale local row exists, the stale row is
/// used: an existing game stays usable while IGDB is down, and only a genuinely
/// unknown game requires a successful upstream call.
pub async fn ensure_game(pool: &PgPool, igdb: &IgdbClient, igdb_id: i64) -> ApiResult<Game> {
    if let Some(fresh) = find_game(pool, igdb_id, true).await? {
        return Ok(fresh);
    }
    match igdb.get(igdb_id).await {
        Ok(upstream) => upsert_game(pool, upstream).await,
        Err(error) => match find_game(pool, igdb_id, false).await? {
            Some(stale) => {
                tracing::warn!(
                    igdb_id,
                    code = error.code,
                    "Serving stale metadata; upstream refresh failed"
                );
                Ok(stale)
            }
            None => Err(error),
        },
    }
}

/// Looks up a game by its upstream ID, optionally requiring metadata refreshed
/// within the last 24 hours.
pub async fn find_game(pool: &PgPool, igdb_id: i64, fresh_only: bool) -> ApiResult<Option<Game>> {
    let row = sqlx::query_as::<_, Game>(
        "SELECT * FROM games WHERE igdb_id=$1 AND (NOT $2 OR refreshed_at > now() - interval '24 hours')",
    )
    .bind(igdb_id)
    .bind(fresh_only)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Writes one upstream game to the catalog, refreshing it if already present.
///
/// `RETURNING *` yields the post-upsert row, so a refresh preserves the local
/// UUID and every library entry and playthrough that references it.
pub async fn upsert_game(pool: &PgPool, game: ProviderGame) -> ApiResult<Game> {
    let row = sqlx::query_as::<_, Game>(
        "INSERT INTO games (id,igdb_id,name,slug,summary,cover_url,first_release_date,platforms,genres,developers,game_type,parent_igdb_id,version_parent_igdb_id,source_url)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (igdb_id) DO UPDATE SET name=EXCLUDED.name,slug=EXCLUDED.slug,summary=EXCLUDED.summary,
         cover_url=EXCLUDED.cover_url,first_release_date=EXCLUDED.first_release_date,platforms=EXCLUDED.platforms,
         genres=EXCLUDED.genres,developers=EXCLUDED.developers,game_type=EXCLUDED.game_type,
         parent_igdb_id=EXCLUDED.parent_igdb_id,version_parent_igdb_id=EXCLUDED.version_parent_igdb_id,
         source_url=EXCLUDED.source_url,refreshed_at=now() RETURNING *")
        .bind(Uuid::new_v4()).bind(game.id).bind(&game.name).bind(&game.slug).bind(&game.summary)
        .bind(game.cover_url()).bind(game.release_date()).bind(Json(&game.platforms)).bind(Json(&game.genres))
        .bind(Json(game.developers())).bind(game.game_type).bind(game.parent_game).bind(game.version_parent)
        .bind(game.source_url())
        .fetch_one(pool).await?;
    Ok(row)
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PlayStatus {
    Backlog,
    Playing,
    Completed,
    OnHold,
    Dropped,
}

#[derive(Serialize, FromRow)]
pub struct LibraryEntry {
    pub igdb_id: i64,
    pub status: PlayStatus,
    pub rating: Option<i16>,
    pub rating_playthrough_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, FromRow)]
pub struct Playthrough {
    pub id: Uuid,
    pub igdb_id: i64,
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, FromRow)]
pub struct Profile {
    pub display_name: String,
    pub bio: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
