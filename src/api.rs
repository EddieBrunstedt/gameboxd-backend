use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use chrono::NaiveDate;
use doorman::prelude::AuthUser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    igdb::{CatalogGame, validate_search},
    models::{Game, LibraryEntry, PlayStatus, Playthrough, Profile, ensure_game, find_game},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me", get(me).put(update_profile))
        .route("/api/games", get(games))
        .route("/api/games/{igdb_id}", get(game))
        .route("/api/catalog/search", get(search))
        .route("/api/catalog/import/{igdb_id}", post(import))
        .route(
            "/api/me/playthroughs",
            get(playthroughs).post(create_playthrough),
        )
        .route(
            "/api/me/playthroughs/{id}",
            get(playthrough)
                .put(update_playthrough)
                .delete(remove_playthrough),
        )
        .route("/api/me/library", get(library))
        .route(
            "/api/me/library/{igdb_id}",
            get(entry).put(save_entry).delete(remove_entry),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    pub q: Option<String>,
    pub status: Option<PlayStatus>,
}
fn default_limit() -> u32 {
    20
}

impl PageQuery {
    pub fn validate(&self) -> ApiResult<()> {
        if !(1..=50).contains(&self.limit) || self.offset > 10_000 {
            return Err(ApiError::invalid(
                "limit must be 1–50 and offset at most 10000",
            ));
        }
        if let Some(q) = &self.q {
            validate_search(q)?;
        }
        Ok(())
    }

    /// The `q` filter as a LIKE pattern, wildcards escaped so a term containing
    /// `%`, `_` or `\` still matches literally. LIKE rather than `strpos`
    /// because only LIKE can be served by the trigram index on `lower(name)`;
    /// the escape character is PostgreSQL's default backslash.
    fn name_pattern(&self) -> Option<String> {
        let q = self.q.as_deref()?.trim();
        let mut pattern = String::with_capacity(q.len() + 2);
        pattern.push('%');
        for ch in q.chars() {
            if matches!(ch, '\\' | '%' | '_') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        pattern.push('%');
        Some(pattern)
    }
}

#[derive(Serialize)]
struct Page<T> {
    data: Vec<T>,
    limit: u32,
    offset: u32,
    has_more: bool,
}

fn page<T>(mut data: Vec<T>, limit: u32, offset: u32) -> Json<Page<T>> {
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);
    Json(Page {
        data,
        limit,
        offset,
        has_more,
    })
}

fn query(value: Result<Query<PageQuery>, QueryRejection>) -> ApiResult<PageQuery> {
    let Query(value) = value.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    value.validate()?;
    Ok(value)
}
fn path<T>(value: Result<Path<T>, PathRejection>) -> ApiResult<T> {
    value
        .map(|Path(value)| value)
        .map_err(|_| ApiError::invalid("Invalid resource ID"))
}
/// Extracts an upstream game ID from the path. Games are addressed publicly by
/// their IGDB ID: it is assigned by the source of truth, is known to a client
/// before the game exists locally, and is stable across databases. The local
/// UUID is an internal primary key and never appears in a URL.
fn igdb_path(value: Result<Path<i64>, PathRejection>) -> ApiResult<i64> {
    let id = path(value)?;
    if id <= 0 {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    Ok(id)
}
fn body<T>(value: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    value.map(|Json(value)| value).map_err(|error| {
        ApiError::new(
            error.status(),
            "invalid_json",
            "Request body does not match the expected JSON schema",
        )
    })
}

async fn games(
    State(state): State<AppState>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Game>>> {
    let params = query(params)?;
    let rows = sqlx::query_as::<_, Game>("SELECT * FROM games WHERE ($1::text IS NULL OR lower(name) LIKE lower($1)) ORDER BY lower(name),id LIMIT $2 OFFSET $3")
        .bind(params.name_pattern()).bind(i64::from(params.limit) + 1).bind(i64::from(params.offset))
        .fetch_all(&state.pool).await?;
    Ok(page(rows, params.limit, params.offset))
}

async fn game(
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<Game>> {
    let row = find_game(&state.pool, igdb_path(igdb_id)?, false)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

async fn search(
    _user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let params = query(params)?;
    let q = params
        .q
        .as_deref()
        .ok_or_else(|| ApiError::invalid("q is required"))?;
    let found = state.igdb.search(q, params.limit, params.offset).await?;
    // Read-only on purpose: a game is stored when a user commits to it, not
    // when anyone searches. Callers name results by igdb_id from here on.
    let data: Vec<CatalogGame> = found.into_iter().map(CatalogGame::from).collect();
    // IGDB has no total in this response. A full page means another page MAY exist.
    let next_offset = (data.len() == params.limit as usize)
        .then_some(params.offset + params.limit)
        .filter(|offset| *offset <= 10_000);
    Ok(Json(
        json!({ "data": data, "limit": params.limit, "offset": params.offset, "next_offset": next_offset }),
    ))
}

async fn import(
    _user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<Game>> {
    let game = ensure_game(&state.pool, &state.igdb, igdb_path(igdb_id)?).await?;
    Ok(Json(game))
}

async fn me(user: AuthUser, State(state): State<AppState>) -> ApiResult<Json<Value>> {
    sqlx::query("INSERT INTO profiles (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(user.0.id.0.as_bytes().as_slice())
        .execute(&state.pool)
        .await?;
    let profile: Profile = sqlx::query_as(
        "SELECT display_name,bio,created_at,updated_at FROM profiles WHERE user_id=$1",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(
        json!({"id": user.0.id, "email": user.0.email, "profile": profile}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileInput {
    display_name: String,
    bio: String,
}

async fn update_profile(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Json<ProfileInput>, JsonRejection>,
) -> ApiResult<Json<Profile>> {
    let input = body(payload)?;
    if input.display_name.trim().chars().count() > 80
        || input.bio.chars().count() > 1000
        || input.display_name.contains('\0')
        || input.bio.contains('\0')
    {
        return Err(ApiError::invalid(
            "display_name is limited to 80 characters and bio to 1000; neither may contain NUL",
        ));
    }
    let profile = sqlx::query_as("INSERT INTO profiles (user_id,display_name,bio) VALUES ($1,$2,$3)
        ON CONFLICT (user_id) DO UPDATE SET display_name=EXCLUDED.display_name,bio=EXCLUDED.bio,updated_at=now()
        RETURNING display_name,bio,created_at,updated_at")
        .bind(user.0.id.0.as_bytes().as_slice()).bind(input.display_name.trim()).bind(&input.bio)
        .fetch_one(&state.pool).await?;
    Ok(Json(profile))
}

/// One row of a library listing: the stored game, plus this user's entry for
/// it. Both halves are the same types `GET /api/games/{igdb_id}` and
/// `GET /api/me/library/{igdb_id}` return, so a column added to either table
/// is published only once it is added to a struct — never implicitly, and
/// never by one route without the other.
#[derive(Serialize, FromRow)]
struct LibraryRow {
    #[sqlx(flatten)]
    game: Game,
    #[sqlx(flatten)]
    entry: LibraryEntry,
}

async fn library(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<LibraryRow>>> {
    let params = query(params)?;
    let name = params.name_pattern();
    let data: Vec<LibraryRow> = sqlx::query_as(
        "SELECT g.id,g.igdb_id,g.name,g.slug,g.summary,g.cover_url,g.first_release_date,
            g.platforms,g.genres,g.developers,g.game_type,g.parent_igdb_id,g.version_parent_igdb_id,
            g.source_url,g.imported_at,g.refreshed_at,
            e.status,e.rating,e.rating_playthrough_id,e.created_at,e.updated_at
        FROM library_entry_details e JOIN games g ON g.id=e.game_id WHERE e.user_id=$1
        AND ($2::text IS NULL OR e.status=$2) AND ($3::text IS NULL OR lower(g.name) LIKE lower($3))
        ORDER BY e.updated_at DESC,e.game_id LIMIT $4 OFFSET $5",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(params.status)
    .bind(name)
    .bind(i64::from(params.limit) + 1)
    .bind(i64::from(params.offset))
    .fetch_all(&state.pool)
    .await?;
    Ok(page(data, params.limit, params.offset))
}

async fn entry(
    user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<LibraryEntry>> {
    let row = sqlx::query_as(
        "SELECT g.igdb_id,e.status,e.rating,e.rating_playthrough_id,e.created_at,e.updated_at
        FROM library_entry_details e JOIN games g ON g.id=e.game_id WHERE e.user_id=$1 AND g.igdb_id=$2",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(igdb_path(igdb_id)?)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryInput {
    pub status: PlayStatus,
}

async fn save_entry(
    user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
    payload: Result<Json<EntryInput>, JsonRejection>,
) -> ApiResult<Json<LibraryEntry>> {
    let igdb_id = igdb_path(igdb_id)?;
    let input = body(payload)?;
    // Adding a game is the point at which it earns a local row, so this is the
    // only library route that may import. Existing metadata follows the shared
    // refresh policy, with stale fallback when the provider is unavailable.
    let game = ensure_game(&state.pool, &state.igdb, igdb_id).await?;
    let mut transaction = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO library_entries (user_id,game_id,status) VALUES ($1,$2,$3)
        ON CONFLICT (user_id,game_id) DO UPDATE SET status=EXCLUDED.status,updated_at=now()",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(game.id)
    .bind(input.status)
    .execute(&mut *transaction)
    .await?;
    let row = sqlx::query_as(
        "SELECT igdb_id,status,rating,rating_playthrough_id,created_at,updated_at
        FROM library_entry_details WHERE user_id=$1 AND game_id=$2",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(game.id)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Json(row))
}

async fn remove_entry(
    user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<StatusCode> {
    // A game that was never imported has no entry to remove, so this stays a
    // silent 204 without consulting upstream.
    sqlx::query(
        "DELETE FROM library_entries WHERE user_id=$1
         AND game_id=(SELECT id FROM games WHERE igdb_id=$2)",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(igdb_path(igdb_id)?)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughInput {
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl PlaythroughInput {
    pub fn validate(&self) -> ApiResult<()> {
        if self
            .rating
            .is_some_and(|rating| !(1..=10).contains(&rating))
        {
            return Err(ApiError::invalid(
                "rating must be an integer from 1 to 10, or null",
            ));
        }
        if self
            .review
            .as_ref()
            .is_some_and(|review| review.chars().count() > 10_000 || review.contains('\0'))
        {
            return Err(ApiError::invalid(
                "review is limited to 10000 characters and may not contain NUL",
            ));
        }
        if let (Some(start), Some(finish)) = (self.started_on, self.finished_on)
            && finish < start
        {
            return Err(ApiError::invalid(
                "finished_on must be on or after started_on",
            ));
        }
        if self.platform.as_ref().is_some_and(|platform| {
            platform.trim().is_empty()
                || platform.chars().count() > 100
                || platform.chars().any(char::is_control)
        }) {
            return Err(ApiError::invalid(
                "platform must contain 1–100 characters without control characters",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePlaythroughInput {
    igdb_id: i64,
    #[serde(flatten)]
    entry: PlaythroughInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaythroughQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
    igdb_id: Option<i64>,
}

async fn playthroughs(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<PlaythroughQuery>, QueryRejection>,
) -> ApiResult<Json<Page<Playthrough>>> {
    let Query(params) = params.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    if !(1..=50).contains(&params.limit) || params.offset > 10_000 {
        return Err(ApiError::invalid(
            "limit must be 1–50 and offset at most 10000",
        ));
    }
    if params.igdb_id.is_some_and(|id| id <= 0) {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    let rows = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p JOIN games g ON g.id=p.game_id
         WHERE p.user_id=$1 AND ($2::bigint IS NULL OR g.igdb_id=$2)
         ORDER BY COALESCE(p.finished_on,p.started_on,(p.created_at AT TIME ZONE 'UTC')::date) DESC,
                  p.created_at DESC,p.id DESC LIMIT $3 OFFSET $4",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(params.igdb_id)
    .bind(i64::from(params.limit) + 1)
    .bind(i64::from(params.offset))
    .fetch_all(&state.pool)
    .await?;
    Ok(page(rows, params.limit, params.offset))
}

async fn playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<Playthrough>> {
    let row = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p JOIN games g ON g.id=p.game_id
         WHERE p.user_id=$1 AND p.id=$2",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(path(id)?)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

async fn create_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Json<CreatePlaythroughInput>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Playthrough>)> {
    let input = body(payload)?;
    if input.igdb_id <= 0 {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    input.entry.validate()?;
    let game = ensure_game(&state.pool, &state.igdb, input.igdb_id).await?;
    let row = sqlx::query_as(
        "INSERT INTO playthroughs (id,user_id,game_id,rating,review,started_on,finished_on,platform)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *, $9::bigint AS igdb_id",
    )
    .bind(Uuid::new_v4()).bind(user.0.id.0.as_bytes().as_slice()).bind(game.id)
    .bind(input.entry.rating).bind(input.entry.review).bind(input.entry.started_on)
    .bind(input.entry.finished_on).bind(input.entry.platform.as_deref().map(str::trim))
    .bind(input.igdb_id).fetch_one(&state.pool).await?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn update_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    payload: Result<Json<PlaythroughInput>, JsonRejection>,
) -> ApiResult<Json<Playthrough>> {
    let id = path(id)?;
    let input = body(payload)?;
    input.validate()?;
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET rating=$3,review=$4,started_on=$5,finished_on=$6,platform=$7,updated_at=now()
         FROM games g WHERE p.user_id=$1 AND p.id=$2 AND g.id=p.game_id
         RETURNING p.*,g.igdb_id",
    )
    .bind(user.0.id.0.as_bytes().as_slice()).bind(id).bind(input.rating).bind(input.review)
    .bind(input.started_on).bind(input.finished_on).bind(input.platform.as_deref().map(str::trim))
    .fetch_optional(&state.pool).await?.ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

async fn remove_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM playthroughs WHERE user_id=$1 AND id=$2")
        .bind(user.0.id.0.as_bytes().as_slice())
        .bind(path(id)?)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
