//! Sharing's authorization boundary is in the public_* database views. Detail
//! reads below and future aggregates/feeds must use those views; an
//! authenticated visitor (including the owner) gets no visibility bypass.
use super::*;
use crate::models::Visibility;
use notes::{TaggedNote, locked_note};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/publication", axum::routing::put(set_publication))
        .route(
            "/api/me/library/{igdb_id}/visibility",
            axum::routing::put(set_library_visibility),
        )
        .route(
            "/api/me/playthroughs/{id}/visibility",
            axum::routing::put(set_playthrough_visibility),
        )
        .route(
            "/api/me/notes/{id}/visibility",
            axum::routing::put(set_note_visibility),
        )
        .route("/api/public/library/{id}", get(library_item))
        .route("/api/public/playthroughs/{id}", get(summary))
        .route("/api/public/ratings/{id}", get(rating))
        .route("/api/public/reviews/{id}", get(review))
        .route("/api/public/notes/{id}", get(note))
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct PublicationInput {
    pub public_enabled: bool,
    // Omission/null preserves the existing explicit bio choice.
    #[ts(optional = nullable)]
    pub bio_visibility: Option<Visibility>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct VisibilityInput {
    pub visibility: Visibility,
}

/// All three choices are required; a partial form cannot silently reset one.
#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct PlaythroughVisibilityInput {
    pub summary_visibility: Visibility,
    pub rating_visibility: Visibility,
    pub review_visibility: Visibility,
}

async fn set_publication(
    user: AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Body<PublicationInput>, ApiError>,
) -> ApiResult<Json<Profile>> {
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    let version = sqlx::query_scalar("SELECT version FROM profiles WHERE user_id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(version) {
        return Err(stale());
    }
    let row = sqlx::query_as("UPDATE profiles SET public_enabled=$2,bio_visibility=COALESCE($3,bio_visibility) WHERE user_id=$1 RETURNING *")
        .bind(owner)
        .bind(input.public_enabled)
        .bind(input.bio_visibility)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row))
}

async fn set_library_visibility(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<VisibilityInput>, ApiError>,
) -> ApiResult<Json<LibraryEntry>> {
    let id = igdb_path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock_igdb_game(&mut tx, owner, id).await?;
    let version = sqlx::query_scalar(
        "SELECT e.version FROM library_entries e JOIN games g ON g.id=e.game_id
         WHERE e.user_id=$1 AND g.igdb_id=$2 FOR UPDATE OF e",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(version) {
        return Err(stale());
    }
    sqlx::query("UPDATE library_entries SET visibility=$3 WHERE user_id=$1 AND game_id=(SELECT id FROM games WHERE igdb_id=$2)")
        .bind(owner).bind(id).bind(input.visibility).execute(&mut *tx).await?;
    let row = sqlx::query_as("SELECT * FROM library_entry_details WHERE user_id=$1 AND igdb_id=$2")
        .bind(owner)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row))
}

async fn set_playthrough_visibility(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<PlaythroughVisibilityInput>, ApiError>,
) -> ApiResult<Tagged> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock_playthrough_game(&mut tx, owner, id).await?;
    let current: Playthrough = sqlx::query_as(
        "SELECT p.*,g.igdb_id FROM playthroughs p JOIN games g ON g.id=p.game_id
         WHERE p.user_id=$1 AND p.id=$2 FOR NO KEY UPDATE OF p",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(current.version) {
        return Err(stale());
    }
    let mut fields = Vec::new();
    if input.rating_visibility == Visibility::Public && current.rating.is_none() {
        fields.push(FieldError::new(
            "rating_visibility",
            FieldErrorCode::Required,
            "Add a rating before publishing it",
        ));
    }
    if input.review_visibility == Visibility::Public && current.review.is_none() {
        fields.push(FieldError::new(
            "review_visibility",
            FieldErrorCode::Required,
            "Add a review before publishing it",
        ));
    }
    if !fields.is_empty() {
        return Err(ApiError::fields(fields));
    }
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET summary_visibility=$3,rating_visibility=$4,review_visibility=$5
         FROM games g WHERE p.user_id=$1 AND p.id=$2 AND g.id=p.game_id RETURNING p.*,g.igdb_id",
    )
    .bind(owner)
    .bind(id)
    .bind(input.summary_visibility)
    .bind(input.rating_visibility)
    .bind(input.review_visibility)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Tagged(row))
}

async fn set_note_visibility(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<VisibilityInput>, ApiError>,
) -> ApiResult<TaggedNote> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    locked_note(&mut tx, owner, id, &precondition).await?;
    sqlx::query("UPDATE diary_notes SET visibility=$3 WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .bind(input.visibility)
        .execute(&mut *tx)
        .await?;
    let row = notes::saved(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(TaggedNote(row))
}

/// Public game context never contains catalog import/refresh bookkeeping.
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicGame {
    pub igdb_id: i64,
    pub game_name: String,
    pub game_slug: String,
    pub cover_url: Option<String>,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicLibraryEntry {
    pub id: Uuid,
    pub author_name: String,
    pub author_id: Uuid,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub status: PlayStatus,
    pub rating: Option<i16>,
    pub rating_id: Option<Uuid>,
}

async fn library_item(
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicLibraryEntry>> {
    let row = sqlx::query_as("SELECT * FROM public_library_entries WHERE id=$1")
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicPlaythrough {
    pub id: Uuid,
    pub author_name: String,
    pub author_id: Uuid,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub state: Option<PlaythroughState>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
}

async fn summary(
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicPlaythrough>> {
    let row = sqlx::query_as("SELECT * FROM public_playthroughs WHERE id=$1")
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicRating {
    pub id: Uuid,
    pub author_name: String,
    pub author_id: Uuid,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub rating: i16,
}

async fn rating(
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicRating>> {
    let row = sqlx::query_as("SELECT * FROM public_ratings WHERE id=$1")
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicReview {
    pub id: Uuid,
    pub author_name: String,
    pub author_id: Uuid,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub review: String,
}

async fn review(
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicReview>> {
    let row = sqlx::query_as("SELECT * FROM public_reviews WHERE id=$1")
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicNote {
    pub id: Uuid,
    pub author_name: String,
    pub author_id: Uuid,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub text: String,
}

async fn note(
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicNote>> {
    let row = sqlx::query_as("SELECT * FROM public_notes WHERE id=$1")
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}
