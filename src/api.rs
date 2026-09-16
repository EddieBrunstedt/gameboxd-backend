use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        FromRequest, Path, Query, Request, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::NaiveDate;
use doorman::prelude::AuthUser;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use sqlx::FromRow;
use ts_rs::TS;
use uuid::Uuid;

mod browsing;
mod notes;
use browsing::{BrowsePage, BrowseQuery, Kind};
mod profiles;
mod sharing;

use crate::{
    AppState,
    error::{ApiError, ApiResult, ErrorCode, FieldError, FieldErrorCode},
    igdb::{CatalogGame, validate_search},
    models::{
        Game, LibraryEntry, PlayStatus, Playthrough, PlaythroughState, Profile, attach_game,
        ensure_game, find_game,
    },
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(notes::routes())
        .merge(sharing::routes())
        .merge(profiles::routes())
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
        .route("/api/me/playthroughs/start", post(start_playthrough))
        .route(
            "/api/me/playthroughs/{id}/lifecycle",
            post(transition_playthrough),
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

/// A page of locally held records, where one extra row was fetched to learn
/// whether another page exists.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
}

/// A page of upstream catalog results. IGDB returns no total, so a full page
/// only offers the next offset to try; it never promises a further result.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct CatalogPage<T> {
    pub data: Vec<T>,
    pub limit: u32,
    pub offset: u32,
    pub next_offset: Option<u32>,
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
/// A JSON request body that reports *which* field it could not read.
///
/// Axum's `Json` rejects a schema failure with prose only, so a form could
/// learn that a body was wrong but never which control to mark. Deserializing
/// through `serde_path_to_error` keeps the path to the offending value, which
/// becomes the `field` of a `FieldError` alongside the ones our own validation
/// produces. Content-type, size and syntax handling stay as they were.
pub struct Body<T>(pub T);

impl<T, S> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !is_json(request.headers()) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::InvalidJson,
                "Request body must be sent as application/json",
            ));
        }
        // `DefaultBodyLimit` is what turns an oversized body into 413 here;
        // this extractor only forwards the status it already produces.
        let bytes = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                ApiError::new(
                    rejection.status(),
                    ErrorCode::InvalidJson,
                    "Request body could not be read",
                )
            })?;
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
        serde_path_to_error::deserialize(&mut deserializer)
            .map(Body)
            .map_err(|error| schema_error(&error))
    }
}

/// Accepts `application/json` and the `+json` structured-suffix family,
/// ignoring any parameters such as `; charset=utf-8`.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let mime = value.split(';').next().unwrap_or_default().trim();
    mime.eq_ignore_ascii_case("application/json")
        || (mime.len() > "application/+json".len()
            && mime.starts_with("application/")
            && mime.ends_with("+json"))
}

fn schema_error(error: &serde_path_to_error::Error<serde_json::Error>) -> ApiError {
    // Malformed JSON has no field to blame: nothing was parsed to a position a
    // form could point at, so it stays the prose 400 it has always been.
    if matches!(
        error.inner().classify(),
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof
    ) {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidJson,
            "Request body is not valid JSON",
        );
    }
    let rejection = ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidJson,
        "Request body does not match the expected JSON schema",
    );
    match schema_field(&error.path().to_string(), &error.inner().to_string()) {
        Some(field) => rejection.with_field(field),
        None => rejection,
    }
}

/// Names the field a schema failure belongs to.
///
/// A wrong type reaches us with a path to the value. A missing or unknown key
/// does not: the path stops at the struct, because the key serde is complaining
/// about is precisely the one that is not in the document. For those two, the
/// name appears only in serde's message, whose wording is fixed by serde itself
/// and is pinned here by unit tests.
fn schema_field(path: &str, message: &str) -> Option<FieldError> {
    if let Some(name) = quoted_after(message, "missing field ") {
        return Some(FieldError::new(
            name,
            FieldErrorCode::Required,
            "This field is required",
        ));
    }
    if let Some(name) = quoted_after(message, "unknown field ") {
        return Some(FieldError::new(
            name,
            FieldErrorCode::UnknownField,
            "This route does not accept this field",
        ));
    }
    // `.` is what serde_path_to_error calls the document root, which is no more
    // use to a form than no field at all.
    (!path.is_empty() && path != ".").then(|| {
        FieldError::new(
            path,
            FieldErrorCode::InvalidFormat,
            "This field has the wrong type or format",
        )
    })
}

fn quoted_after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message
        .strip_prefix(prefix)?
        .strip_prefix('`')?
        .split('`')
        .next()
}

fn body<T>(value: Result<Body<T>, ApiError>) -> ApiResult<T> {
    value.map(|Body(value)| value)
}

async fn games(
    State(state): State<AppState>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<Page<CatalogGame>>> {
    let params = query(params)?;
    let rows = sqlx::query_as::<_, Game>("SELECT * FROM games WHERE ($1::text IS NULL OR lower(name) LIKE lower($1)) ORDER BY lower(name),id LIMIT $2 OFFSET $3")
        .bind(params.name_pattern()).bind(i64::from(params.limit) + 1).bind(i64::from(params.offset))
        .fetch_all(&state.pool).await?;
    Ok(page(
        rows.into_iter().map(CatalogGame::from).collect(),
        params.limit,
        params.offset,
    ))
}

async fn game(
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<CatalogGame>> {
    let row = find_game(&state.pool, igdb_path(igdb_id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(CatalogGame::from(row)))
}

async fn search(
    State(state): State<AppState>,
    params: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<CatalogPage<CatalogGame>>> {
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
    Ok(Json(CatalogPage {
        data,
        limit: params.limit,
        offset: params.offset,
        next_offset,
    }))
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

/// The signed-in user, as the SPA sees them: Doorman's identity plus this
/// application's profile. Doorman's own `UserDto` is richer and is served on
/// its own routes; this route publishes only what the app itself owns.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct Me {
    pub id: Uuid,
    pub email: Option<String>,
    pub profile: Profile,
}

async fn me(user: AuthUser, State(state): State<AppState>) -> ApiResult<Json<Me>> {
    sqlx::query("INSERT INTO profiles (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(user.0.id.0.as_bytes().as_slice())
        .execute(&state.pool)
        .await?;
    let profile: Profile = sqlx::query_as("SELECT * FROM profiles WHERE user_id=$1")
        .bind(user.0.id.0.as_bytes().as_slice())
        .fetch_one(&state.pool)
        .await?;
    Ok(Json(Me {
        id: user.0.id.0,
        email: user.0.email.clone(),
        profile,
    }))
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    pub display_name: String,
    pub bio: String,
}

impl ProfileInput {
    /// Both limits are reported together, so a profile form marks every field
    /// it got wrong in one round trip rather than one per save.
    fn validated(&self) -> ApiResult<()> {
        let mut fields = Vec::new();
        if self.display_name.trim().chars().count() > 80 {
            fields.push(FieldError::new(
                "display_name",
                FieldErrorCode::TooLong,
                "Display name is limited to 80 characters",
            ));
        } else if self.display_name.contains('\0') {
            fields.push(FieldError::new(
                "display_name",
                FieldErrorCode::InvalidFormat,
                "Display name may not contain NUL",
            ));
        }
        if self.bio.chars().count() > 1000 {
            fields.push(FieldError::new(
                "bio",
                FieldErrorCode::TooLong,
                "Bio is limited to 1000 characters",
            ));
        } else if self.bio.contains('\0') {
            fields.push(FieldError::new(
                "bio",
                FieldErrorCode::InvalidFormat,
                "Bio may not contain NUL",
            ));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(ApiError::fields(fields))
        }
    }
}

async fn update_profile(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Body<ProfileInput>, ApiError>,
) -> ApiResult<Json<Profile>> {
    let input = body(payload)?;
    input.validated()?;
    let profile = sqlx::query_as("INSERT INTO profiles (user_id,display_name,bio) VALUES ($1,$2,$3)
        ON CONFLICT (user_id) DO UPDATE SET display_name=EXCLUDED.display_name,bio=EXCLUDED.bio,updated_at=now()
        RETURNING *")
        .bind(user.0.id.0.as_bytes().as_slice()).bind(input.display_name.trim()).bind(&input.bio)
        .fetch_one(&state.pool).await?;
    Ok(Json(profile))
}

/// One row of a library listing: the stored game, plus this user's entry for
/// it. The game matches the authenticated import response; the entry matches
/// `GET /api/me/library/{igdb_id}`. Public game routes deliberately use the
/// descriptive catalog type instead, omitting local bookkeeping.
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct LibraryRow {
    last_played_on: Option<NaiveDate>,
    #[sqlx(flatten)]
    game: Game,
    #[sqlx(flatten)]
    entry: LibraryEntry,
}

async fn library(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<BrowsePage<LibraryRow>>> {
    let params = BrowseQuery::parse(params)?;
    let kind = Kind::Library { public: false };
    params.validate(kind)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    let result = browsing::read_page(
        &mut tx,
        user.0.id.0.as_bytes().as_slice(),
        browsing::OWNER_LIBRARY,
        &params,
        kind,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

async fn entry(
    user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
) -> ApiResult<Json<LibraryEntry>> {
    let row = sqlx::query_as(
        "SELECT g.igdb_id,e.status,e.rating,e.rating_playthrough_id,e.current_playthrough_id,e.created_at,e.updated_at,e.public_id,e.visibility,e.version
        FROM library_entry_details e JOIN games g ON g.id=e.game_id WHERE e.user_id=$1 AND g.igdb_id=$2",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(igdb_path(igdb_id)?)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct EntryInput {
    pub status: PlayStatus,
}

async fn save_entry(
    user: AuthUser,
    State(state): State<AppState>,
    igdb_id: Result<Path<i64>, PathRejection>,
    payload: Result<Body<EntryInput>, ApiError>,
) -> ApiResult<Json<LibraryEntry>> {
    let igdb_id = igdb_path(igdb_id)?;
    let input = body(payload)?;
    // Adding a game is the point at which it earns a local row, so this is the
    // only library route that may import. A game already stored is reused as
    // it stands, so this save is independent of provider latency.
    let game = attach_game(&state.pool, &state.igdb, &state.refresher, igdb_id).await?;
    let mut transaction = state.pool.begin().await?;
    let owner = user.0.id.0.as_bytes().as_slice();
    lock_game(&mut transaction, owner, game.id).await?;
    let current_state: Option<PlaythroughState> = sqlx::query_scalar(
        "SELECT state FROM playthroughs WHERE user_id=$1 AND game_id=$2 AND is_current",
    )
    .bind(owner)
    .bind(game.id)
    .fetch_optional(&mut *transaction)
    .await?;
    if current_state.is_some_and(|current| current.library_status() != input.status) {
        return Err(lifecycle_conflict(
            "Use a lifecycle action while this game has a current run",
        ));
    }
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
        "SELECT *
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
    let igdb_id = igdb_path(igdb_id)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut transaction = state.pool.begin().await?;
    // Serialize even when membership is absent. A concurrent start/import
    // must not escape a delete merely because there was no row to lock yet.
    lock_igdb_game(&mut transaction, owner, igdb_id).await?;
    sqlx::query(
        "UPDATE playthroughs SET is_current=false WHERE user_id=$1
        AND game_id=(SELECT id FROM games WHERE igdb_id=$2) AND is_current",
    )
    .bind(owner)
    .bind(igdb_id)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DELETE FROM library_entries WHERE user_id=$1
        AND game_id=(SELECT id FROM games WHERE igdb_id=$2)",
    )
    .bind(owner)
    .bind(igdb_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
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
    pub state: Option<PlaythroughState>,
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub presentation_on: Option<NaiveDate>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl PlaythroughFields {
    /// Normalizes the free-text label, then checks everything the table also
    /// constrains, so bad input is a 400 here rather than a constraint
    /// violation surfacing as an internal error.
    ///
    /// Every failing field is collected before returning. A playthrough form
    /// shows a rating, a review and two dates at once, and fixing them one
    /// refused save at a time would be a worse form for no benefit — the checks
    /// are pure and cheap.
    pub fn validated(mut self) -> ApiResult<Self> {
        let mut fields = Vec::new();
        if matches!(
            self.state,
            Some(PlaythroughState::Active | PlaythroughState::Paused)
        ) && self.finished_on.is_some()
        {
            fields.push(FieldError::new(
                "finished_on",
                FieldErrorCode::InvalidFormat,
                "Active or paused playthroughs cannot have an end date",
            ));
        }
        if self
            .rating
            .is_some_and(|rating| !(1..=10).contains(&rating))
        {
            fields.push(FieldError::new(
                "rating",
                FieldErrorCode::OutOfRange,
                "rating must be an integer from 1 to 10, or null",
            ));
        }
        if let Some(review) = self.review.as_ref() {
            if review.chars().count() > 10_000 {
                fields.push(FieldError::new(
                    "review",
                    FieldErrorCode::TooLong,
                    "review is limited to 10000 characters",
                ));
            } else if review.contains('\0') {
                fields.push(FieldError::new(
                    "review",
                    FieldErrorCode::InvalidFormat,
                    "review may not contain NUL",
                ));
            }
        }
        // Checked on the merged values, so a partial edit cannot move one date
        // past the stored other one. Reported against `finished_on`: it is the
        // constrained end of the pair, and the one a date picker can mark.
        if let (Some(start), Some(finish)) = (self.started_on, self.finished_on)
            && finish < start
        {
            fields.push(FieldError::new(
                "finished_on",
                FieldErrorCode::BeforeStart,
                "finished_on must be on or after started_on",
            ));
        }
        if let Some(platform) = self.platform.take() {
            // Only spaces are stripped, matching the `btrim` the table's check
            // constraint uses; every other control character stays visible to
            // the rejection below instead of being silently normalized away.
            let platform = platform.trim_matches(' ');
            if platform.is_empty() {
                fields.push(FieldError::new(
                    "platform",
                    FieldErrorCode::Required,
                    "platform must not be blank; send null to clear it",
                ));
            } else if platform.chars().count() > 100 {
                fields.push(FieldError::new(
                    "platform",
                    FieldErrorCode::TooLong,
                    "platform is limited to 100 characters",
                ));
            } else if platform.chars().any(char::is_control) {
                fields.push(FieldError::new(
                    "platform",
                    FieldErrorCode::InvalidFormat,
                    "platform may not contain control characters",
                ));
            } else {
                self.platform = Some(platform.to_owned());
            }
        }
        if fields.is_empty() {
            Ok(self)
        } else {
            Err(ApiError::fields(fields))
        }
    }
}

impl From<&Playthrough> for PlaythroughFields {
    fn from(row: &Playthrough) -> Self {
        Self {
            state: row.state,
            rating: row.rating,
            review: row.review.clone(),
            presentation_on: row.presentation_on,
            started_on: row.started_on,
            finished_on: row.finished_on,
            platform: row.platform.clone(),
        }
    }
}

/// A complete replacement of the editable fields: an omitted field is cleared,
/// exactly as an explicit `null` is.
#[derive(Debug, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughInput {
    pub state: Option<PlaythroughState>,
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub presentation_on: Option<NaiveDate>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl From<PlaythroughInput> for PlaythroughFields {
    fn from(input: PlaythroughInput) -> Self {
        // Destructured rather than field-by-field: a new editable field then
        // fails to compile here until it is actually carried across.
        let PlaythroughInput {
            state,
            rating,
            review,
            presentation_on,
            started_on,
            finished_on,
            platform,
        } = input;
        Self {
            state,
            rating,
            review,
            presentation_on,
            started_on,
            finished_on,
            platform,
        }
    }
}

/// A partial edit: an omitted field keeps its stored value and an explicit
/// `null` clears it. Separate status, rating and review controls can each send
/// only what they own, so one widget cannot erase another's field.
#[derive(Debug, Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughPatch {
    // `optional = nullable` is what carries the three states into TypeScript:
    // the key may be absent, present as `null`, or present with a value, which
    // is exactly what `Patch` distinguishes. `as` names the shape to describe,
    // since `Patch` itself is a Rust-side detail.
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<i16>")]
    pub rating: Patch<i16>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<PlaythroughState>")]
    pub state: Patch<PlaythroughState>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<String>")]
    pub review: Patch<String>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<NaiveDate>")]
    pub presentation_on: Patch<NaiveDate>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<NaiveDate>")]
    pub started_on: Patch<NaiveDate>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<NaiveDate>")]
    pub finished_on: Patch<NaiveDate>,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<String>")]
    pub platform: Patch<String>,
}

impl PlaythroughPatch {
    fn apply(self, current: PlaythroughFields) -> PlaythroughFields {
        PlaythroughFields {
            state: self.state.apply(current.state),
            rating: self.rating.apply(current.rating),
            review: self.review.apply(current.review),
            presentation_on: self.presentation_on.apply(current.presentation_on),
            started_on: self.started_on.apply(current.started_on),
            finished_on: self.finished_on.apply(current.finished_on),
            platform: self.platform.apply(current.platform),
        }
    }
}

/// The create body: a game to record against, plus the same editable fields
/// the replacement body carries.
///
/// Spelled out rather than `#[serde(flatten)]`ing [`PlaythroughInput`], which
/// produces the identical JSON but buffers the flattened keys during
/// deserialization — and a buffered key has no path, so a badly typed `rating`
/// or an impossible `started_on` could not be reported against the field that
/// sent it. The conversion below destructures exhaustively, so the two bodies
/// cannot quietly drift apart.
#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct CreatePlaythroughInput {
    pub igdb_id: i64,
    pub state: Option<PlaythroughState>,
    pub rating: Option<i16>,
    pub review: Option<String>,
    pub presentation_on: Option<NaiveDate>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

impl From<CreatePlaythroughInput> for PlaythroughFields {
    fn from(input: CreatePlaythroughInput) -> Self {
        let CreatePlaythroughInput {
            igdb_id: _,
            state,
            rating,
            review,
            presentation_on,
            started_on,
            finished_on,
            platform,
        } = input;
        Self {
            state,
            rating,
            review,
            presentation_on,
            started_on,
            finished_on,
            platform,
        }
    }
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
            ErrorCode::PreconditionRequired,
            "Send If-Match with the version this edit was based on, so a stale editor cannot overwrite newer work",
        )
    })
}

fn stale() -> ApiError {
    ApiError::new(
        StatusCode::PRECONDITION_FAILED,
        ErrorCode::PreconditionFailed,
        "The record changed since it was read; reload it and reapply the edit",
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

async fn playthroughs(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<BrowsePage<Playthrough>>> {
    let params = BrowseQuery::parse(params)?;
    params.validate(Kind::Playthroughs)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    let result = browsing::read_page(
        &mut tx,
        user.0.id.0.as_bytes().as_slice(),
        browsing::OWNER_PLAYTHROUGHS,
        &params,
        Kind::Playthroughs,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
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
    payload: Result<Body<CreatePlaythroughInput>, ApiError>,
) -> ApiResult<(StatusCode, Tagged)> {
    let input = body(payload)?;
    if input.igdb_id <= 0 {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    let igdb_id = input.igdb_id;
    let fields = PlaythroughFields::from(input).validated()?;
    let game = attach_game(&state.pool, &state.igdb, &state.refresher, igdb_id).await?;
    let row = sqlx::query_as(
        "INSERT INTO playthroughs (id,user_id,game_id,rating,review,started_on,finished_on,platform,presentation_on,state)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *, $11::bigint AS igdb_id",
    )
    .bind(Uuid::new_v4()).bind(user.0.id.0.as_bytes().as_slice()).bind(game.id)
    .bind(fields.rating).bind(fields.review).bind(fields.started_on)
    .bind(fields.finished_on).bind(fields.platform).bind(fields.presentation_on).bind(fields.state)
    .bind(igdb_id).fetch_one(&state.pool).await?;
    Ok((StatusCode::CREATED, Tagged(row)))
}

async fn replace_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<PlaythroughInput>, ApiError>,
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
    payload: Result<Body<PlaythroughPatch>, ApiError>,
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
    lock_playthrough_game(&mut transaction, owner, id).await?;
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
    if current.is_current && fields.state != current.state {
        return Err(lifecycle_conflict(
            "Use a lifecycle action to change the current run's state",
        ));
    }
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET rating=$3,review=$4,started_on=$5,finished_on=$6,platform=$7,presentation_on=$8,state=$9
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
    .bind(fields.presentation_on)
    .bind(fields.state)
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
    let precondition = if_match(&headers)?;
    let mut transaction = state.pool.begin().await?;
    if !lock_playthrough_game(&mut transaction, owner, id).await? {
        return if precondition.is_some() {
            Err(stale())
        } else {
            Ok(StatusCode::NO_CONTENT)
        };
    }
    let Some(precondition) = precondition else {
        // Unconditional deletion stays idempotent: asking for a playthrough to
        // be gone succeeds when it already is.
        sqlx::query("DELETE FROM playthroughs WHERE user_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
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
    .execute(&mut *transaction)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(stale());
    }
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn lifecycle_conflict(message: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, ErrorCode::LifecycleConflict, message)
}

/// All writes that can affect the current run use the same transaction lock,
/// before taking any playthrough row lock. It exists even without membership.
/// Hash collisions only serialize unrelated games; they cannot weaken isolation.
async fn lock_igdb_game(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    igdb_id: i64,
) -> ApiResult<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
        'gameboxd:lifecycle:' || encode($1::bytea,'hex') || ':' || $2::bigint::text, 0))",
    )
    .bind(owner)
    .bind(igdb_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn lock_game(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    game_id: Uuid,
) -> ApiResult<()> {
    let igdb_id = sqlx::query_scalar("SELECT igdb_id FROM games WHERE id=$1")
        .bind(game_id)
        .fetch_one(&mut **transaction)
        .await?;
    lock_igdb_game(transaction, owner, igdb_id).await
}

async fn lock_playthrough_game(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    id: Uuid,
) -> ApiResult<bool> {
    let game_id = sqlx::query_scalar("SELECT game_id FROM playthroughs WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .fetch_optional(&mut **transaction)
        .await?;
    if let Some(game_id) = game_id {
        lock_game(transaction, owner, game_id).await?;
        return Ok(true);
    }
    Ok(false)
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct StartPlaythroughInput {
    pub igdb_id: i64,
    pub started_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

async fn start_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Body<StartPlaythroughInput>, ApiError>,
) -> ApiResult<(StatusCode, Tagged)> {
    let input = body(payload)?;
    if input.igdb_id <= 0 {
        return Err(ApiError::invalid("IGDB ID must be positive"));
    }
    let fields = PlaythroughFields {
        state: Some(PlaythroughState::Active),
        rating: None,
        review: None,
        presentation_on: None,
        started_on: input.started_on,
        finished_on: None,
        platform: input.platform,
    }
    .validated()?;
    let game = attach_game(&state.pool, &state.igdb, &state.refresher, input.igdb_id).await?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut transaction = state.pool.begin().await?;
    lock_game(&mut transaction, owner, game.id).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playthroughs
        WHERE user_id=$1 AND game_id=$2 AND is_current)",
    )
    .bind(owner)
    .bind(game.id)
    .fetch_one(&mut *transaction)
    .await?;
    if exists {
        return Err(lifecycle_conflict(
            "This game already has a current run; finish or abandon it before starting a replay",
        ));
    }
    let row = sqlx::query_as(
        "INSERT INTO playthroughs
        (id,user_id,game_id,state,is_current,started_on,platform)
        VALUES ($1,$2,$3,'active',true,$4,$5) RETURNING *, $6::bigint AS igdb_id",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .bind(game.id)
    .bind(fields.started_on)
    .bind(fields.platform)
    .bind(input.igdb_id)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO library_entries (user_id,game_id,status) VALUES ($1,$2,'playing')
        ON CONFLICT (user_id,game_id) DO UPDATE SET status='playing',updated_at=now()",
    )
    .bind(owner)
    .bind(game.id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok((StatusCode::CREATED, Tagged(row)))
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Pause,
    Resume,
    Complete,
    Abandon,
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct LifecycleInput {
    pub action: LifecycleAction,
    // End date for complete/abandon. Never defaults to the recording day.
    pub finished_on: Option<NaiveDate>,
}

async fn transition_playthrough(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<LifecycleInput>, ApiError>,
) -> ApiResult<Tagged> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut transaction = state.pool.begin().await?;
    lock_playthrough_game(&mut transaction, owner, id).await?;
    let current: Playthrough = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p
        JOIN games g ON g.id=p.game_id WHERE p.user_id=$1 AND p.id=$2 FOR NO KEY UPDATE OF p",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(current.version) {
        return Err(stale());
    }
    if !current.is_current {
        return Err(lifecycle_conflict(
            "This is not the current run; correct historical state through PUT or PATCH",
        ));
    }
    let next = match (current.state, input.action) {
        (Some(PlaythroughState::Active), LifecycleAction::Pause) => PlaythroughState::Paused,
        (Some(PlaythroughState::Paused), LifecycleAction::Resume) => PlaythroughState::Active,
        (Some(PlaythroughState::Active | PlaythroughState::Paused), LifecycleAction::Complete) => {
            PlaythroughState::Completed
        }
        (Some(PlaythroughState::Active | PlaythroughState::Paused), LifecycleAction::Abandon) => {
            PlaythroughState::Abandoned
        }
        _ => {
            return Err(lifecycle_conflict(
                "The action is not valid for the current state",
            ));
        }
    };
    let mut fields = PlaythroughFields::from(&current);
    fields.state = Some(next);
    fields.finished_on = input.finished_on;
    let fields = fields.validated()?;
    let is_current = matches!(next, PlaythroughState::Active | PlaythroughState::Paused);
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET state=$3,is_current=$4,finished_on=$5
        FROM games g WHERE p.user_id=$1 AND p.id=$2 AND g.id=p.game_id RETURNING p.*,g.igdb_id",
    )
    .bind(owner)
    .bind(id)
    .bind(next)
    .bind(is_current)
    .bind(fields.finished_on)
    .fetch_one(&mut *transaction)
    .await?;
    // A missing library row would indicate an invariant violation; do not
    // commit half a transition in that case.
    sqlx::query_scalar::<_, Uuid>(
        "UPDATE library_entries SET status=$3,updated_at=now()
        WHERE user_id=$1 AND game_id=(SELECT game_id FROM playthroughs WHERE id=$2)
        RETURNING game_id",
    )
    .bind(owner)
    .bind(id)
    .bind(next.library_status())
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(Tagged(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            state: None,
            rating: Some(8),
            review: Some("Stored".into()),
            presentation_on: None,
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
            state: None,
            rating: None,
            review: None,
            presentation_on: None,
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

    /// Runs a body through the same deserializer the extractor uses, so these
    /// assertions pin serde's real wording rather than an assumption about it.
    fn schema_rejection<T: DeserializeOwned>(body: &str) -> ApiError {
        let mut deserializer = serde_json::Deserializer::from_slice(body.as_bytes());
        let error = serde_path_to_error::deserialize::<_, T>(&mut deserializer)
            .err()
            .expect("body should have been rejected");
        schema_error(&error)
    }

    fn only_field(error: &ApiError) -> &FieldError {
        match error.fields.as_slice() {
            [field] => field,
            other => panic!("expected exactly one field error, got {other:?}"),
        }
    }

    #[test]
    fn a_schema_failure_names_the_field_that_caused_it() {
        // A missing or unknown key is not at a path, because the key serde is
        // complaining about is the one absent from the document.
        let missing = schema_rejection::<CreatePlaythroughInput>("{}");
        assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(missing.code, ErrorCode::InvalidJson);
        assert_eq!(only_field(&missing).field, "igdb_id");
        assert_eq!(only_field(&missing).code, FieldErrorCode::Required);

        let unknown =
            schema_rejection::<CreatePlaythroughInput>(r#"{"igdb_id":42,"user_id":"someone"}"#);
        assert_eq!(only_field(&unknown).field, "user_id");
        assert_eq!(only_field(&unknown).code, FieldErrorCode::UnknownField);

        // A wrong type does have a path, and it is the path that is used.
        let wrong_type = schema_rejection::<CreatePlaythroughInput>(r#"{"igdb_id":"forty-two"}"#);
        assert_eq!(only_field(&wrong_type).field, "igdb_id");
        assert_eq!(only_field(&wrong_type).code, FieldErrorCode::InvalidFormat);

        // A date that parses as a string but is not a real calendar date is a
        // failure of the field, not of the document.
        let bad_date = schema_rejection::<CreatePlaythroughInput>(
            r#"{"igdb_id":42,"started_on":"2026-02-30"}"#,
        );
        assert_eq!(only_field(&bad_date).field, "started_on");
        assert_eq!(only_field(&bad_date).code, FieldErrorCode::InvalidFormat);

        // A patch names its field too, even though every key is optional there.
        let patch = schema_rejection::<PlaythroughPatch>(r#"{"rating":"nine"}"#);
        assert_eq!(only_field(&patch).field, "rating");
    }

    #[test]
    fn malformed_json_stays_a_document_level_failure() {
        // Nothing parsed far enough to blame a field, and inventing one would
        // point a form at a control that may not be the problem at all.
        for body in ["{", "", "not json", r#"{"igdb_id":42,}"#] {
            let error = schema_rejection::<CreatePlaythroughInput>(body);
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{body:?}");
            assert_eq!(error.code, ErrorCode::InvalidJson, "{body:?}");
            assert!(error.fields.is_empty(), "{body:?}");
        }
    }

    #[test]
    fn json_content_types_are_accepted_with_parameters_and_suffixes() {
        let header = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            headers
        };
        for value in [
            "application/json",
            "application/json; charset=utf-8",
            "APPLICATION/JSON",
            "application/merge-patch+json",
        ] {
            assert!(is_json(&header(value)), "{value}");
        }
        for value in ["text/plain", "application/xml", "application/+json", ""] {
            assert!(!is_json(&header(value)), "{value}");
        }
        assert!(!is_json(&HeaderMap::new()));
    }

    #[test]
    fn every_invalid_field_is_reported_from_one_request() {
        // A form showing four controls should not have to discover its errors
        // one refused save at a time.
        let error = PlaythroughFields {
            state: None,
            rating: Some(11),
            review: Some("x".repeat(10_001)),
            presentation_on: None,
            started_on: NaiveDate::from_ymd_opt(2026, 2, 1),
            finished_on: NaiveDate::from_ymd_opt(2026, 1, 1),
            platform: Some("  ".to_owned()),
        }
        .validated()
        .expect_err("every field is invalid");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, ErrorCode::ValidationError);
        let reported: Vec<_> = error
            .fields
            .iter()
            .map(|field| (field.field.as_str(), field.code))
            .collect();
        assert_eq!(
            reported,
            [
                ("rating", FieldErrorCode::OutOfRange),
                ("review", FieldErrorCode::TooLong),
                ("finished_on", FieldErrorCode::BeforeStart),
                ("platform", FieldErrorCode::Required),
            ]
        );
        // The summary stays useful for a caller that renders no field detail.
        assert_eq!(error.message, "4 fields are invalid");

        let profile = ProfileInput {
            display_name: "n".repeat(81),
            bio: "b".repeat(1001),
        }
        .validated()
        .expect_err("both profile fields are too long");
        assert_eq!(
            profile
                .fields
                .iter()
                .map(|field| field.field.as_str())
                .collect::<Vec<_>>(),
            ["display_name", "bio"]
        );

        // A single failure keeps its own message as the summary, so nothing
        // reads worse than it did before field errors existed.
        let single = PlaythroughFields {
            state: None,
            rating: Some(0),
            review: None,
            presentation_on: None,
            started_on: None,
            finished_on: None,
            platform: None,
        }
        .validated()
        .expect_err("rating is out of range");
        assert_eq!(
            single.message,
            "rating must be an integer from 1 to 10, or null"
        );
    }
}
