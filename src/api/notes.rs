use super::*;
use chrono::{DateTime, Utc};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/diary", get(diary))
        .route("/api/me/playthroughs/{id}/notes", get(list).post(create))
        .route(
            "/api/me/notes/{id}",
            get(read).put(replace).patch(update).delete(remove),
        )
}

/// Private owner representation, including independent note bookkeeping.
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct DiaryNote {
    pub visibility: crate::models::Visibility,
    pub id: Uuid,
    pub playthrough_id: Uuid,
    pub igdb_id: i64,
    pub game_name: String,
    pub game_slug: String,
    pub cover_url: Option<String>,
    pub text: String,
    pub presentation_on: Option<NaiveDate>,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, TS, Serialize)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct NoteInput {
    pub text: String,
    pub presentation_on: Option<NaiveDate>,
}

impl NoteInput {
    fn validated(self) -> ApiResult<Self> {
        let error = if self.text.trim().is_empty() {
            Some((FieldErrorCode::Required, "text must not be blank"))
        } else if self.text.chars().count() > 10_000 {
            Some((
                FieldErrorCode::TooLong,
                "text is limited to 10000 characters",
            ))
        } else if self.text.contains('\0') {
            Some((FieldErrorCode::InvalidFormat, "text may not contain NUL"))
        } else {
            None
        };
        if let Some((code, message)) = error {
            return Err(ApiError::fields(vec![FieldError::new(
                "text", code, message,
            )]));
        }
        Ok(self)
    }
}

#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct NotePatch {
    // A present text must be a string; unlike the date, it cannot be cleared.
    #[serde(default)]
    #[ts(optional, as = "Option<String>")]
    pub text: NoteTextPatch,
    #[serde(default)]
    #[ts(optional = nullable, as = "Option<NaiveDate>")]
    pub presentation_on: Patch<NaiveDate>,
}

#[derive(Default)]
pub struct NoteTextPatch(Option<String>);

impl<'de> Deserialize<'de> for NoteTextPatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| Self(Some(text)))
    }
}

pub(super) struct TaggedNote(pub(super) DiaryNote);

impl IntoResponse for TaggedNote {
    fn into_response(self) -> Response {
        let etag = HeaderValue::try_from(format!("\"{}\"", self.0.version))
            .expect("a decimal version is a valid header value");
        ([(header::ETAG, etag)], Json(self.0)).into_response()
    }
}

async fn note_page(
    state: &AppState,
    owner: &[u8],
    parent: Option<Uuid>,
    params: BrowseQuery,
) -> ApiResult<Json<BrowsePage<DiaryNote>>> {
    params.validate(Kind::OwnerNotes)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    if let Some(parent) = parent {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM playthroughs WHERE user_id=$1 AND id=$2")
            .bind(owner)
            .bind(parent)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    }
    let result = browsing::read_page(
        &mut tx,
        owner,
        browsing::OWNER_NOTES,
        &params,
        Kind::OwnerNotes,
        parent,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

async fn diary(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<BrowsePage<DiaryNote>>> {
    note_page(
        &state,
        user.0.id.0.as_bytes().as_slice(),
        None,
        BrowseQuery::parse(params)?,
    )
    .await
}

async fn list(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<BrowsePage<DiaryNote>>> {
    note_page(
        &state,
        user.0.id.0.as_bytes().as_slice(),
        Some(path(id)?),
        BrowseQuery::parse(params)?,
    )
    .await
}

async fn read(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<TaggedNote> {
    let row = sqlx::query_as("SELECT * FROM diary_note_details WHERE user_id=$1 AND id=$2")
        .bind(user.0.id.0.as_bytes().as_slice())
        .bind(path(id)?)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(TaggedNote(row))
}

// Lock the parent before the note, as a cascading parent deletion does. KEY
// SHARE blocks deletion but permits lifecycle/content updates. Notes neither
// change current-run state nor increment the parent's independent revision.
async fn lock_parent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    id: Uuid,
) -> ApiResult<()> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM playthroughs WHERE user_id=$1 AND id=$2 FOR KEY SHARE",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(())
}

async fn create(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<NoteInput>, ApiError>,
) -> ApiResult<Response> {
    let parent = path(id)?;
    let input = body(payload)?;
    let receipt = retry::Receipt::new(&headers, &format!("note:create:{parent}"), &input)?;
    let input = input.validated()?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    if let Some(response) = receipt.replay(&mut tx, user.0.id.0.as_bytes()).await? {
        return Ok(response);
    }
    lock_parent(&mut tx, owner, parent).await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO diary_notes (id,user_id,playthrough_id,text,presentation_on) VALUES ($1,$2,$3,$4,$5)")
        .bind(id).bind(owner).bind(parent).bind(input.text).bind(input.presentation_on)
        .execute(&mut *tx).await?;
    let row = saved(&mut tx, owner, id).await?;
    receipt
        .finish(
            tx,
            user.0.id.0.as_bytes(),
            (StatusCode::CREATED, TaggedNote(row)),
        )
        .await
}

pub(super) async fn saved(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    id: Uuid,
) -> ApiResult<DiaryNote> {
    Ok(
        sqlx::query_as("SELECT * FROM diary_note_details WHERE user_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?,
    )
}

pub(super) async fn locked_note(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    owner: &[u8],
    id: Uuid,
    precondition: &Precondition,
) -> ApiResult<DiaryNote> {
    let parent =
        sqlx::query_scalar("SELECT playthrough_id FROM diary_notes WHERE user_id=$1 AND id=$2")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    lock_parent(tx, owner, parent).await?;
    let version: i32 =
        sqlx::query_scalar("SELECT version FROM diary_notes WHERE user_id=$1 AND id=$2 FOR UPDATE")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(version) {
        return Err(stale());
    }
    saved(tx, owner, id).await
}

async fn replace(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<NoteInput>, ApiError>,
) -> ApiResult<TaggedNote> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    edit(&state, &user, id, &precondition, |_| input).await
}

async fn update(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<NotePatch>, ApiError>,
) -> ApiResult<TaggedNote> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let input = body(payload)?;
    edit(&state, &user, id, &precondition, |current| NoteInput {
        text: input.text.0.unwrap_or(current.text),
        presentation_on: input.presentation_on.apply(current.presentation_on),
    })
    .await
}

async fn edit(
    state: &AppState,
    user: &AuthUser,
    id: Uuid,
    precondition: &Precondition,
    merge: impl FnOnce(DiaryNote) -> NoteInput,
) -> ApiResult<TaggedNote> {
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    let current = locked_note(&mut tx, owner, id, precondition).await?;
    let input = merge(current).validated()?;
    sqlx::query("UPDATE diary_notes SET text=$3,presentation_on=$4 WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .bind(input.text)
        .bind(input.presentation_on)
        .execute(&mut *tx)
        .await?;
    let row = saved(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(TaggedNote(row))
}

async fn remove(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let id = path(id)?;
    let precondition = required_precondition(&headers)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    locked_note(&mut tx, owner, id, &precondition).await?;
    sqlx::query("DELETE FROM diary_notes WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
