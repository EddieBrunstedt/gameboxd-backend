use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, types::Json};
use ts_rs::TS;
use uuid::Uuid;

use crate::{
    error::ApiResult,
    igdb::{IgdbClient, NamedItem, ProviderGame},
    refresh::Refresher,
};

/// How long stored metadata is treated as current before a refresh is due.
pub const METADATA_TTL: TimeDelta = TimeDelta::hours(24);

#[derive(Debug, Serialize, FromRow, TS)]
#[ts(export)]
pub struct Game {
    pub id: Uuid,
    pub igdb_id: i64,
    pub name: String,
    pub slug: String,
    pub summary: Option<String>,
    pub cover_url: Option<String>,
    pub first_release_date: Option<DateTime<Utc>>,
    // The `Json` wrapper is how sqlx reads a jsonb column; it is transparent
    // on the wire, so the published type is the array it contains.
    #[ts(as = "Vec<NamedItem>")]
    pub platforms: Json<Vec<NamedItem>>,
    #[ts(as = "Vec<NamedItem>")]
    pub genres: Json<Vec<NamedItem>>,
    #[ts(as = "Vec<NamedItem>")]
    pub developers: Json<Vec<NamedItem>>,
    pub game_type: Option<i64>,
    pub parent_igdb_id: Option<i64>,
    pub version_parent_igdb_id: Option<i64>,
    pub source_url: String,
    pub imported_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
}

impl Game {
    /// Whether stored metadata is still current.
    ///
    /// Decided here rather than in the query so the rule has a single
    /// definition, and so a caller can tell "stale" apart from "not stored" —
    /// the two now call for different things.
    pub fn is_fresh(&self) -> bool {
        Utc::now().signed_duration_since(self.refreshed_at) < METADATA_TTL
    }
}

/// The local row for an upstream game, importing or refreshing it in line.
///
/// This is the explicit-import path, where waiting for IGDB is the point of
/// the request: stale metadata is refreshed before the caller is answered. If
/// upstream is unavailable but a stale local row exists, the stale row is
/// served, so an import of a game already stored still succeeds while IGDB is
/// down. Only a genuinely unknown game requires a successful upstream call.
///
/// Personal records take [`attach_game`] instead, which never waits.
pub async fn ensure_game(pool: &PgPool, igdb: &IgdbClient, igdb_id: i64) -> ApiResult<Game> {
    let Some(stored) = find_game(pool, igdb_id).await? else {
        return upsert_game(pool, igdb.get(igdb_id).await?).await;
    };
    if stored.is_fresh() {
        return Ok(stored);
    }
    match igdb.get(igdb_id).await {
        Ok(upstream) => upsert_game(pool, upstream).await,
        Err(error) => {
            tracing::warn!(
                igdb_id,
                code = error.code.as_str(),
                "Serving stale metadata; upstream refresh failed"
            );
            Ok(stored)
        }
    }
}

/// The local row a personal record — a library entry or a playthrough —
/// attaches to.
///
/// A stored game is used immediately, however old its metadata, and a stale
/// one is queued for a background refresh instead. Saving a game the user
/// already has therefore never waits for IGDB: not for its request spacing,
/// not for its queue, and not for a timeout while it is down. Only a game that
/// was never stored requires a successful upstream call, because there is
/// otherwise no row for the record to reference.
///
/// Materializing a game stays a side effect of an operation the user actually
/// intends — adding it to a library, recording a playthrough, or an explicit
/// import — never of a search or a catalog lookup.
pub async fn attach_game(
    pool: &PgPool,
    igdb: &IgdbClient,
    refresher: &Refresher,
    igdb_id: i64,
) -> ApiResult<Game> {
    if let Some(stored) = find_game(pool, igdb_id).await? {
        if !stored.is_fresh() {
            refresher.request(igdb_id);
        }
        return Ok(stored);
    }
    upsert_game(pool, igdb.get(igdb_id).await?).await
}

/// The locally stored game for an upstream ID, however old its metadata.
/// Callers decide what staleness means to them through [`Game::is_fresh`].
pub async fn find_game(pool: &PgPool, igdb_id: i64) -> ApiResult<Option<Game>> {
    let row = sqlx::query_as::<_, Game>("SELECT * FROM games WHERE igdb_id=$1")
        .bind(igdb_id)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, sqlx::Type, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PlayStatus {
    Backlog,
    Playing,
    Completed,
    OnHold,
    Dropped,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct LibraryEntry {
    pub public_id: Uuid,
    pub visibility: Visibility,
    pub version: i32,
    pub current_playthrough_id: Option<Uuid>,
    pub igdb_id: i64,
    pub status: PlayStatus,
    pub rating: Option<i16>,
    pub rating_playthrough_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Explicit experience state; null on a playthrough means the outcome is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, sqlx::Type, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum PlaythroughState {
    Active,
    Paused,
    Completed,
    Abandoned,
}

impl PlaythroughState {
    pub fn library_status(self) -> PlayStatus {
        match self {
            Self::Active => PlayStatus::Playing,
            Self::Paused => PlayStatus::OnHold,
            Self::Completed => PlayStatus::Completed,
            Self::Abandoned => PlayStatus::Dropped,
        }
    }
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct Playthrough {
    pub summary_visibility: Visibility,
    pub rating_visibility: Visibility,
    pub review_visibility: Visibility,
    pub state: Option<PlaythroughState>,
    pub is_current: bool,
    pub id: Uuid,
    pub igdb_id: i64,
    // Revision counter, incremented by the database on every real change.
    // Published so a client holding a listed playthrough can edit it without
    // re-reading the single-resource route for its `ETag`.
    pub version: i32,
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub presentation_on: Option<NaiveDate>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct Profile {
    pub public_enabled: bool,
    pub version: i32,
    pub display_name: String,
    pub bio: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Owner-controlled permission; creation always uses the database's private default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Public,
}

// Accept only the documented string form. Serde's derived enum decoder also
// accepts externally tagged objects and classifies null as a syntax error.
impl<'de> Deserialize<'de> for Visibility {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            value => Err(serde::de::Error::unknown_variant(
                value,
                &["private", "public"],
            )),
        }
    }
}
