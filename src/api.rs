use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::NaiveDate;
use doorman::prelude::AuthUser;
use serde::{Deserialize, Deserializer, Serialize};
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
        .route("/api/catalog/games/{igdb_id}", get(catalog_game))
        .route("/api/catalog/import/{igdb_id}", post(import))
        .route(
            "/api/me/playthroughs",
            get(playthroughs).post(create_playthrough),
        )
        .route(
            "/api/me/playthroughs/{id}",
            get(playthrough)
                .put(replace_playthrough)
                .patch(update_playthrough)
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

async fn catalog_game(
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<CatalogGame>> {
    // Like search, viewing a game never imports or refreshes a local row.
    let game = state.igdb.get(igdb_path(igdb_id)?).await?;
    Ok(Json(CatalogGame::from(game)))
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

/// A field in a PATCH body. Absence, `null` and a value are three different
/// instructions, which `Option<T>` alone cannot express.
#[derive(Debug)]
pub enum Patch<T> {
    /// The field was omitted: keep whatever is stored.
    Keep,
    /// The field was explicitly `null`: clear the stored value.
    Clear,
    /// The field carried a value: store it.
    Set(T),
}

// Written by hand rather than derived: deriving `Default` on a generic enum
// would demand `T: Default` from every patchable field type, which absence of a
// field has nothing to do with.
#[expect(
    clippy::derivable_impls,
    reason = "a derive would add a T: Default bound"
)]
impl<T> Default for Patch<T> {
    fn default() -> Self {
        Self::Keep
    }
}

impl<T> Patch<T> {
    fn apply(self, current: Option<T>) -> Option<T> {
        match self {
            Self::Keep => current,
            Self::Clear => None,
            Self::Set(value) => Some(value),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Only a field that is present reaches this point; `#[serde(default)]`
        // on each field turns an omitted one into `Keep`.
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

/// The editable half of a playthrough. Replacement and partial edits both
/// resolve to one of these before validation and the write, so the two routes
/// cannot drift apart in what they accept or how they normalize it.
#[derive(Debug)]
pub struct PlaythroughFields {
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl PlaythroughFields {
    /// Normalizes the free-text label, then checks everything the table also
    /// constrains, so bad input is a 400 here rather than a constraint
    /// violation surfacing as an internal error.
    pub fn validated(mut self) -> ApiResult<Self> {
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
        // Checked on the merged values, so a partial edit cannot move one date
        // past the stored other one.
        if let (Some(start), Some(finish)) = (self.started_on, self.finished_on)
            && finish < start
        {
            return Err(ApiError::invalid(
                "finished_on must be on or after started_on",
            ));
        }
        if let Some(platform) = self.platform.take() {
            // Only spaces are stripped, matching the `btrim` the table's check
            // constraint uses; every other control character stays visible to
            // the rejection below instead of being silently normalized away.
            let platform = platform.trim_matches(' ');
            if platform.is_empty()
                || platform.chars().count() > 100
                || platform.chars().any(char::is_control)
            {
                return Err(ApiError::invalid(
                    "platform must contain 1–100 characters without control characters",
                ));
            }
            self.platform = Some(platform.to_owned());
        }
        Ok(self)
    }
}

impl From<&Playthrough> for PlaythroughFields {
    fn from(row: &Playthrough) -> Self {
        Self {
            rating: row.rating,
            review: row.review.clone(),
            started_on: row.started_on,
            finished_on: row.finished_on,
            platform: row.platform.clone(),
        }
    }
}

/// A complete replacement of the editable fields: an omitted field is cleared,
/// exactly as an explicit `null` is.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughInput {
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl From<PlaythroughInput> for PlaythroughFields {
    fn from(input: PlaythroughInput) -> Self {
        Self {
            rating: input.rating,
            review: input.review,
            started_on: input.started_on,
            finished_on: input.finished_on,
            platform: input.platform,
        }
    }
}

/// A partial edit: an omitted field keeps its stored value and an explicit
/// `null` clears it. Separate status, rating and review controls can each send
/// only what they own, so one widget cannot erase another's field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughPatch {
    #[serde(default)]
    pub rating: Patch<i16>,
    #[serde(default)]
    pub review: Patch<String>,
    #[serde(default)]
    pub started_on: Patch<NaiveDate>,
    #[serde(default)]
    pub finished_on: Patch<NaiveDate>,
    #[serde(default)]
    pub platform: Patch<String>,
}

impl PlaythroughPatch {
    fn apply(self, current: PlaythroughFields) -> PlaythroughFields {
        PlaythroughFields {
            rating: self.rating.apply(current.rating),
            review: self.review.apply(current.review),
            started_on: self.started_on.apply(current.started_on),
            finished_on: self.finished_on.apply(current.finished_on),
            platform: self.platform.apply(current.platform),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePlaythroughInput {
    igdb_id: i64,
    #[serde(flatten)]
    entry: PlaythroughInput,
}

/// An `If-Match` precondition, parsed from the request headers.
#[derive(Debug)]
enum Precondition {
    /// `*`: any revision, as long as the playthrough still exists.
    Any,
    /// Entity tags. Weak tags are dropped during parsing, because `If-Match`
    /// is defined to compare strongly and a weak tag can therefore never
    /// satisfy it; a request carrying only weak tags fails the precondition.
    Tags(Vec<String>),
}

impl Precondition {
    /// The versions this precondition accepts. A tag that is not the decimal
    /// rendering of a version can never name a revision of this resource, so it
    /// is discarded rather than compared; the round-trip check keeps that
    /// equivalent to the octet comparison RFC 9110 specifies.
    fn versions(&self) -> Vec<i32> {
        match self {
            Self::Any => Vec::new(),
            Self::Tags(tags) => tags
                .iter()
                .filter_map(|tag| {
                    tag.parse::<i32>()
                        .ok()
                        .filter(|version| version.to_string() == *tag)
                })
                .collect(),
        }
    }

    fn accepts_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    fn is_met_by(&self, version: i32) -> bool {
        self.accepts_any() || self.versions().contains(&version)
    }
}

/// Parses `If-Match` (RFC 9110 section 13.1.1), returning `None` when the header
/// is absent. A header this server cannot evaluate is rejected rather than
/// silently treated as no precondition at all, which would be the one outcome
/// the caller was trying to avoid.
fn if_match(headers: &HeaderMap) -> ApiResult<Option<Precondition>> {
    let values = headers.get_all(header::IF_MATCH);
    if values.iter().next().is_none() {
        return Ok(None);
    }
    let malformed = || {
        ApiError::invalid(
            r#"If-Match must be "*" or a comma-separated list of entity tags, such as "3""#,
        )
    };
    let mut tags = Vec::new();
    let mut any = false;
    let mut items = 0;
    for value in values {
        for item in value.to_str().map_err(|_| malformed())?.split(',') {
            let item = item.trim();
            items += 1;
            if item == "*" {
                any = true;
                continue;
            }
            let strong = item.strip_prefix("W/").unwrap_or(item);
            let opaque = strong
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .ok_or_else(malformed)?;
            if opaque.contains('"') {
                return Err(malformed());
            }
            if strong == item {
                tags.push(opaque.to_owned());
            }
        }
    }
    // The grammar allows `*` only on its own, never mixed with entity tags.
    if any && items > 1 {
        return Err(malformed());
    }
    Ok(Some(if any {
        Precondition::Any
    } else {
        Precondition::Tags(tags)
    }))
}

/// The precondition an edit must carry. Editing without one is refused with 428
/// rather than silently allowed: the lost update it would permit is exactly what
/// the entity tag exists to prevent, and every editor has already read the
/// playthrough it is about to change.
fn required_precondition(headers: &HeaderMap) -> ApiResult<Precondition> {
    if_match(headers)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition_required",
            "Send If-Match with the version this edit was based on, so a stale editor cannot overwrite newer work",
        )
    })
}

fn stale() -> ApiError {
    ApiError::new(
        StatusCode::PRECONDITION_FAILED,
        "precondition_failed",
        "The playthrough changed since it was read; reload it and reapply the edit",
    )
}

/// One playthrough plus the `ETag` naming its current revision, which a later
/// edit echoes in `If-Match`.
struct Tagged(Playthrough);

impl IntoResponse for Tagged {
    fn into_response(self) -> Response {
        let etag = HeaderValue::try_from(format!("\"{}\"", self.0.version))
            .expect("a decimal version is a valid header value");
        ([(header::ETAG, etag)], Json(self.0)).into_response()
    }
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
) -> ApiResult<Tagged> {
    let row = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p JOIN games g ON g.id=p.game_id
         WHERE p.user_id=$1 AND p.id=$2",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(path(id)?)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Tagged(row))
}

async fn create_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Json<CreatePlaythroughInput>, JsonRejection>,
) -> ApiResult<(StatusCode, Tagged)> {
    let input = body(payload)?;
    if input.igdb_id <= 0 {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    let fields = PlaythroughFields::from(input.entry).validated()?;
    let game = ensure_game(&state.pool, &state.igdb, input.igdb_id).await?;
    let row = sqlx::query_as(
        "INSERT INTO playthroughs (id,user_id,game_id,rating,review,started_on,finished_on,platform)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *, $9::bigint AS igdb_id",
    )
    .bind(Uuid::new_v4()).bind(user.0.id.0.as_bytes().as_slice()).bind(game.id)
    .bind(fields.rating).bind(fields.review).bind(fields.started_on)
    .bind(fields.finished_on).bind(fields.platform)
    .bind(input.igdb_id).fetch_one(&state.pool).await?;
    Ok((StatusCode::CREATED, Tagged(row)))
}

async fn replace_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Json<PlaythroughInput>, JsonRejection>,
) -> ApiResult<Tagged> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    edit_playthrough(&state, &user, id, &precondition, |_| {
        PlaythroughFields::from(input)
    })
    .await
}

async fn update_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Json<PlaythroughPatch>, JsonRejection>,
) -> ApiResult<Tagged> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    edit_playthrough(&state, &user, id, &precondition, |current| {
        input.apply(current)
    })
    .await
}

/// Reads, merges and rewrites one playthrough under a row lock.
///
/// The lock makes the whole read–modify–write atomic, so a partial edit merges
/// into the revision it was checked against; a concurrent editor waits and then
/// sees the committed result rather than merging into a value that no longer
/// exists. The revision trigger leaves an edit that changes nothing untouched,
/// so re-saving an unmodified form does not invalidate another tab's tag.
async fn edit_playthrough(
    state: &AppState,
    user: &AuthUser,
    id: Uuid,
    precondition: &Precondition,
    merge: impl FnOnce(PlaythroughFields) -> PlaythroughFields,
) -> ApiResult<Tagged> {
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut transaction = state.pool.begin().await?;
    // Ownership is resolved before the precondition, so another user's
    // playthrough is a 404 and never reveals a revision through 412.
    let current: Playthrough = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p JOIN games g ON g.id=p.game_id
         WHERE p.user_id=$1 AND p.id=$2 FOR NO KEY UPDATE OF p",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(current.version) {
        return Err(stale());
    }
    let fields = merge(PlaythroughFields::from(&current)).validated()?;
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET rating=$3,review=$4,started_on=$5,finished_on=$6,platform=$7
         FROM games g WHERE p.user_id=$1 AND p.id=$2 AND g.id=p.game_id
         RETURNING p.*,g.igdb_id",
    )
    .bind(owner)
    .bind(id)
    .bind(fields.rating)
    .bind(fields.review)
    .bind(fields.started_on)
    .bind(fields.finished_on)
    .bind(fields.platform)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Tagged(row))
}

async fn remove_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let id = path(id)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let Some(precondition) = if_match(&headers)? else {
        // Unconditional deletion stays idempotent: asking for a playthrough to
        // be gone succeeds when it already is.
        sqlx::query("DELETE FROM playthroughs WHERE user_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .execute(&state.pool)
            .await?;
        return Ok(StatusCode::NO_CONTENT);
    };
    // A conditional deletion asserts a specific revision instead, so a
    // playthrough that is absent, someone else's, or already edited fails the
    // precondition rather than reporting the success above.
    let deleted = sqlx::query(
        "DELETE FROM playthroughs WHERE user_id=$1 AND id=$2
         AND ($3::boolean OR version = ANY($4::integer[]))",
    )
    .bind(owner)
    .bind(id)
    .bind(precondition.accepts_any())
    .bind(precondition.versions())
    .execute(&state.pool)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(stale());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::IF_MATCH, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn parse(values: &[&str]) -> Precondition {
        if_match(&headers(values))
            .expect("well-formed If-Match")
            .expect("present If-Match")
    }

    #[test]
    fn if_match_distinguishes_absence_from_an_unusable_header() {
        assert!(if_match(&HeaderMap::new()).unwrap().is_none());
        for malformed in [
            "",         // present but empty
            "   ",      // whitespace only
            "3",        // a bare version is not an entity tag
            r#""3","#,  // trailing empty element
            r#"*,"3""#, // `*` is only valid on its own
            r#""a"b""#, // an unescaped quote inside the opaque tag
            r#""3"#,    // unterminated
            "W/3",      // weak marker without an opaque tag
        ] {
            assert!(
                if_match(&headers(&[malformed])).is_err(),
                "{malformed:?} must be rejected"
            );
        }
        let mut binary = HeaderMap::new();
        binary.insert(header::IF_MATCH, HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(if_match(&binary).is_err());
    }

    #[test]
    fn if_match_compares_entity_tags_strongly() {
        assert!(parse(&["*"]).is_met_by(1));
        assert!(parse(&["*"]).is_met_by(i32::MAX));
        assert!(parse(&[r#""3""#]).is_met_by(3));
        assert!(!parse(&[r#""3""#]).is_met_by(4));
        // A list matches any of its members, across folded or repeated headers.
        assert!(parse(&[r#""1", "3""#]).is_met_by(3));
        assert!(parse(&[r#""1""#, r#""3""#]).is_met_by(1));
        assert!(!parse(&[r#""1""#, r#""3""#]).is_met_by(2));
        // Weak tags parse, but strong comparison can never accept one.
        assert!(!parse(&[r#"W/"3""#]).is_met_by(3));
        assert!(parse(&[r#"W/"3", "3""#]).is_met_by(3));
        // Tags that merely parse as the same number are different octets.
        for equivalent in [r#""007""#, r#""+7""#, r#"" 7""#, r#""7 ""#] {
            assert!(!parse(&[equivalent]).is_met_by(7), "{equivalent}");
        }
        // A tag naming no revision of anything simply never matches.
        assert!(!parse(&[r#""deadbeef""#]).is_met_by(1));
        assert!(parse(&[r#""deadbeef""#]).versions().is_empty());
    }

    #[test]
    fn a_patch_keeps_clears_and_sets_each_field_independently() {
        let stored = || PlaythroughFields {
            rating: Some(8),
            review: Some("Stored".into()),
            started_on: NaiveDate::from_ymd_opt(2026, 1, 1),
            finished_on: NaiveDate::from_ymd_opt(2026, 2, 1),
            platform: Some("PC".into()),
        };
        let patch = |value| serde_json::from_value::<PlaythroughPatch>(value).unwrap();

        // An empty patch is a no-op, not a replacement with nothing.
        let kept = patch(json!({})).apply(stored());
        assert_eq!(kept.rating, Some(8));
        assert_eq!(kept.review.as_deref(), Some("Stored"));
        assert_eq!(kept.platform.as_deref(), Some("PC"));
        assert_eq!(kept.started_on, stored().started_on);

        // A review editor that sends only its own field cannot touch a rating.
        let edited = patch(json!({"review":"Edited"})).apply(stored());
        assert_eq!(edited.review.as_deref(), Some("Edited"));
        assert_eq!(edited.rating, Some(8));

        // Explicit null is the only way to clear a value.
        let cleared = patch(json!({"rating":null,"platform":null})).apply(stored());
        assert_eq!(cleared.rating, None);
        assert_eq!(cleared.platform, None);
        assert_eq!(cleared.review.as_deref(), Some("Stored"));

        // Replacement keeps its own contract: an omitted field is cleared.
        let replaced = PlaythroughFields::from(
            serde_json::from_value::<PlaythroughInput>(json!({"rating":5})).unwrap(),
        );
        assert_eq!(replaced.rating, Some(5));
        assert_eq!(replaced.review, None);
        assert_eq!(replaced.platform, None);

        // Typos are refused rather than silently changing nothing.
        for unknown in [json!({"ratings":5}), json!({"igdb_id":42})] {
            assert!(serde_json::from_value::<PlaythroughPatch>(unknown).is_err());
        }
    }

    #[test]
    fn merged_fields_are_validated_and_normalized_together() {
        let stored = PlaythroughFields {
            rating: None,
            review: None,
            started_on: NaiveDate::from_ymd_opt(2026, 3, 1),
            finished_on: None,
            platform: None,
        };
        let patch = |value| serde_json::from_value::<PlaythroughPatch>(value).unwrap();
        // The finish is compared against the stored start, which this patch omits.
        assert!(
            patch(json!({"finished_on":"2026-01-01"}))
                .apply(stored)
                .validated()
                .is_err()
        );
        let trimmed = patch(json!({"platform":" Steam Deck "}))
            .apply(PlaythroughFields::from(
                serde_json::from_value::<PlaythroughInput>(json!({})).unwrap(),
            ))
            .validated()
            .unwrap();
        assert_eq!(trimmed.platform.as_deref(), Some("Steam Deck"));
    }
}
