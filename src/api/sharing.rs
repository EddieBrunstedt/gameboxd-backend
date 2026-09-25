//! Sharing's authorization boundary is in the public_* database views. Detail
//! reads below and future aggregates/feeds must use those views; an
//! authenticated visitor (including the owner) gets no visibility bypass.
use super::*;
use crate::models::{SharingDefaults, Visibility};
use notes::{TaggedNote, locked_note};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/publication", axum::routing::put(set_publication))
        .route("/api/me/visibility", axum::routing::put(set_all_visibility))
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
    // Omission/null preserves the explicit summary choice.
    #[ts(optional = nullable)]
    pub statistics_visibility: Option<Visibility>,
    /// Omission/null preserves the stored defaults. When present, all six are
    /// required, so a partial form cannot silently reset one. Existing content
    /// is never changed; see `PUT /api/me/visibility` for that.
    #[ts(optional = nullable)]
    pub sharing_defaults: Option<SharingDefaults>,
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

/// Visibility sent alongside a playthrough's content. An omitted choice means
/// the owner's default for a component this write brings into existence, and
/// the stored choice for one that already existed.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct RunChoice {
    pub summary: Option<Visibility>,
    pub rating: Option<Visibility>,
    pub review: Option<Visibility>,
}

/// The three visibilities a playthrough is written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunVisibility {
    pub summary: Visibility,
    pub rating: Visibility,
    pub review: Visibility,
}

impl RunChoice {
    /// A component without content cannot be published. An explicit request to
    /// publish one is a field error; a public default simply has nothing to
    /// apply to (see [`RunVisibility::resolve`]).
    pub(super) fn validate(&self, after: &PlaythroughFields) -> ApiResult<()> {
        let mut fields = Vec::new();
        if self.rating == Some(Visibility::Public) && after.rating.is_none() {
            fields.push(FieldError::new(
                "rating_visibility",
                FieldErrorCode::Required,
                "Add a rating before publishing it",
            ));
        }
        if self.review == Some(Visibility::Public) && after.review.is_none() {
            fields.push(FieldError::new(
                "review_visibility",
                FieldErrorCode::Required,
                "Add a review before publishing it",
            ));
        }
        if fields.is_empty() {
            Ok(())
        } else {
            Err(ApiError::fields(fields))
        }
    }
}

impl RunVisibility {
    /// What a write stores. `before` is the stored run, or `None` when the write
    /// creates one; `after` is the content being written. The choice must
    /// already have passed [`RunChoice::validate`].
    ///
    /// A rating or review that gains content here is new, so it takes the
    /// owner's default; one that already had content keeps its choice, so
    /// editing a review never changes who can read it. A component without
    /// content is always private.
    pub(super) fn resolve(
        choice: RunChoice,
        defaults: SharingDefaults,
        before: Option<&Playthrough>,
        after: &PlaythroughFields,
    ) -> Self {
        let component = |choice: Option<Visibility>,
                         stored: Option<Visibility>,
                         present: bool,
                         default: Visibility| {
            if present {
                choice.or(stored).unwrap_or(default)
            } else {
                Visibility::Private
            }
        };
        Self {
            summary: choice
                .summary
                .or(before.map(|run| run.summary_visibility))
                .unwrap_or(defaults.summary),
            rating: component(
                choice.rating,
                before
                    .filter(|run| run.rating.is_some())
                    .map(|run| run.rating_visibility),
                after.rating.is_some(),
                defaults.rating,
            ),
            review: component(
                choice.review,
                before
                    .filter(|run| run.review.is_some())
                    .map(|run| run.review_visibility),
                after.review.is_some(),
                defaults.review,
            ),
        }
    }
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
    let defaults = input.sharing_defaults;
    let row = sqlx::query_as(
        "UPDATE profiles SET public_enabled=$2,
            bio_visibility=COALESCE($3,bio_visibility),
            statistics_visibility=COALESCE($4,statistics_visibility),
            default_library_visibility=COALESCE($5,default_library_visibility),
            default_summary_visibility=COALESCE($6,default_summary_visibility),
            default_rating_visibility=COALESCE($7,default_rating_visibility),
            default_review_visibility=COALESCE($8,default_review_visibility),
            default_note_visibility=COALESCE($9,default_note_visibility),
            default_list_visibility=COALESCE($10,default_list_visibility)
         WHERE user_id=$1 RETURNING *",
    )
    .bind(owner)
    .bind(input.public_enabled)
    .bind(input.bio_visibility)
    .bind(input.statistics_visibility)
    .bind(defaults.map(|d| d.library))
    .bind(defaults.map(|d| d.summary))
    .bind(defaults.map(|d| d.rating))
    .bind(defaults.map(|d| d.review))
    .bind(defaults.map(|d| d.note))
    .bind(defaults.map(|d| d.list))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(row))
}

/// Sets every existing item of each named kind to one visibility. A kind left
/// out is untouched. Sending the owner's defaults applies them retroactively.
#[derive(Deserialize, TS)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct BulkVisibilityInput {
    #[serde(default)]
    #[ts(optional)]
    pub library: Option<Visibility>,
    #[serde(default)]
    #[ts(optional)]
    pub summary: Option<Visibility>,
    /// Applies only to playthroughs that have a rating.
    #[serde(default)]
    #[ts(optional)]
    pub rating: Option<Visibility>,
    /// Applies only to playthroughs that have a review.
    #[serde(default)]
    #[ts(optional)]
    pub review: Option<Visibility>,
    #[serde(default)]
    #[ts(optional)]
    pub note: Option<Visibility>,
    #[serde(default)]
    #[ts(optional)]
    pub list: Option<Visibility>,
}

/// How many items of each kind changed. Items already at the requested
/// visibility, and kinds left out, count zero.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct BulkVisibilityReport {
    pub library: u64,
    pub summary: u64,
    pub rating: u64,
    pub review: u64,
    pub note: u64,
    pub list: u64,
}

/// One transaction for the whole change, so no reader sees half of it.
///
/// Row locks are taken table by table in the order single-item writers use:
/// playthroughs before their notes (notes lock their parent first) and before
/// library entries (lifecycle actions update the run, then the entry). A
/// concurrent single-item write therefore waits or is waited on, never both.
/// Changed rows advance their own revisions, so an editor holding an older
/// tag gets 412 rather than silently reverting this change.
async fn set_all_visibility(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Body<BulkVisibilityInput>, ApiError>,
) -> ApiResult<Json<BulkVisibilityReport>> {
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    async fn apply(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        sql: &'static str,
        owner: &[u8],
        visibility: Option<Visibility>,
    ) -> ApiResult<u64> {
        let Some(visibility) = visibility else {
            return Ok(0);
        };
        Ok(sqlx::query(sql)
            .bind(owner)
            .bind(visibility)
            .execute(&mut **tx)
            .await?
            .rows_affected())
    }
    let summary = apply(
        &mut tx,
        "UPDATE playthroughs SET summary_visibility=$2 WHERE user_id=$1 AND summary_visibility<>$2",
        owner,
        input.summary,
    )
    .await?;
    let rating = apply(
        &mut tx,
        "UPDATE playthroughs SET rating_visibility=$2
         WHERE user_id=$1 AND rating IS NOT NULL AND rating_visibility<>$2",
        owner,
        input.rating,
    )
    .await?;
    let review = apply(
        &mut tx,
        "UPDATE playthroughs SET review_visibility=$2
         WHERE user_id=$1 AND review IS NOT NULL AND review_visibility<>$2",
        owner,
        input.review,
    )
    .await?;
    let note = apply(
        &mut tx,
        "UPDATE diary_notes SET visibility=$2 WHERE user_id=$1 AND visibility<>$2",
        owner,
        input.note,
    )
    .await?;
    let library = apply(
        &mut tx,
        "UPDATE library_entries SET visibility=$2 WHERE user_id=$1 AND visibility<>$2",
        owner,
        input.library,
    )
    .await?;
    let list = apply(
        &mut tx,
        "UPDATE game_lists SET visibility=$2 WHERE user_id=$1 AND visibility<>$2",
        owner,
        input.list,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(BulkVisibilityReport {
        library,
        summary,
        rating,
        review,
        note,
        list,
    }))
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
        "SELECT p.*,g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url FROM playthroughs p JOIN games g ON g.id=p.game_id
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
    RunChoice {
        summary: Some(input.summary_visibility),
        rating: Some(input.rating_visibility),
        review: Some(input.review_visibility),
    }
    .validate(&PlaythroughFields::from(&current))?;
    let row = sqlx::query_as(
        "UPDATE playthroughs p SET summary_visibility=$3,rating_visibility=$4,review_visibility=$5
         FROM games g WHERE p.user_id=$1 AND p.id=$2 AND g.id=p.game_id RETURNING p.*,g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url",
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
    /// The author's current canonical handle, read live; null without one.
    pub author_handle: Option<String>,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub status: PlayStatus,
    pub rating: Option<i16>,
    pub rating_id: Option<Uuid>,
}

async fn library_item(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicLibraryEntry>> {
    let row = sqlx::query_as(
        "SELECT * FROM public_library_entries WHERE id=$1 AND public_view_allowed($2,user_id)",
    )
    .bind(path(id)?)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
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
    /// The author's current canonical handle, read live; null without one.
    pub author_handle: Option<String>,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub state: Option<PlaythroughState>,
    pub started_on: Option<NaiveDate>,
    pub finished_on: Option<NaiveDate>,
    pub platform: Option<String>,
    /// The same run's rating, only when that rating is itself public.
    pub rating: Option<i16>,
}

async fn summary(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicPlaythrough>> {
    let row = sqlx::query_as(
        "SELECT * FROM public_playthroughs WHERE id=$1 AND public_view_allowed($2,user_id)",
    )
    .bind(path(id)?)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
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
    /// The author's current canonical handle, read live; null without one.
    pub author_handle: Option<String>,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub rating: i16,
}

async fn rating(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicRating>> {
    let row = sqlx::query_as(
        "SELECT * FROM public_ratings WHERE id=$1 AND public_view_allowed($2,user_id)",
    )
    .bind(path(id)?)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
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
    /// The author's current canonical handle, read live; null without one.
    pub author_handle: Option<String>,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub review: String,
    /// The same run's rating, only when that rating is itself public.
    pub rating: Option<i16>,
}

async fn review(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicReview>> {
    let row = sqlx::query_as(
        "SELECT * FROM public_reviews WHERE id=$1 AND public_view_allowed($2,user_id)",
    )
    .bind(path(id)?)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
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
    /// The author's current canonical handle, read live; null without one.
    pub author_handle: Option<String>,
    #[sqlx(flatten)]
    pub game: PublicGame,
    pub presentation_on: Option<NaiveDate>,
    pub text: String,
}

async fn note(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicNote>> {
    let row = sqlx::query_as(
        "SELECT * FROM public_notes WHERE id=$1 AND public_view_allowed($2,user_id)",
    )
    .bind(path(id)?)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Visibility::{Private, Public};

    fn content(rating: Option<i16>, review: Option<&str>) -> PlaythroughFields {
        PlaythroughFields {
            state: None,
            rating,
            review: review.map(str::to_owned),
            presentation_on: None,
            started_on: None,
            finished_on: None,
            platform: None,
        }
    }

    fn stored(rating: Option<i16>, review: Option<&str>, visibility: Visibility) -> Playthrough {
        Playthrough {
            summary_visibility: visibility,
            rating_visibility: if rating.is_some() {
                visibility
            } else {
                Private
            },
            review_visibility: if review.is_some() {
                visibility
            } else {
                Private
            },
            state: None,
            is_current: false,
            id: Uuid::nil(),
            igdb_id: 1,
            game_name: String::new(),
            game_slug: String::new(),
            cover_url: None,
            version: 1,
            rating,
            review: review.map(str::to_owned),
            presentation_on: None,
            started_on: None,
            finished_on: None,
            platform: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
            updated_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    const SHARE: SharingDefaults = SharingDefaults {
        library: Public,
        summary: Public,
        rating: Public,
        review: Public,
        note: Private,
        list: Private,
    };

    fn resolve(
        choice: RunChoice,
        defaults: SharingDefaults,
        before: Option<&Playthrough>,
        after: &PlaythroughFields,
    ) -> (Visibility, Visibility, Visibility) {
        choice.validate(after).expect("valid choice");
        let v = RunVisibility::resolve(choice, defaults, before, after);
        (v.summary, v.rating, v.review)
    }

    #[test]
    fn a_new_run_takes_defaults_only_for_components_with_content() {
        let after = content(Some(8), None);
        assert_eq!(
            resolve(RunChoice::default(), SHARE, None, &after),
            (Public, Public, Private)
        );
        assert_eq!(
            resolve(RunChoice::default(), SharingDefaults::PRIVATE, None, &after),
            (Private, Private, Private)
        );
    }

    #[test]
    fn an_explicit_choice_overrides_the_default() {
        let choice = RunChoice {
            review: Some(Private),
            ..RunChoice::default()
        };
        let after = content(Some(8), Some("Spoilers"));
        assert_eq!(
            resolve(choice, SHARE, None, &after),
            (Public, Public, Private)
        );
    }

    #[test]
    fn content_gained_by_an_edit_is_new_and_takes_the_default() {
        // A private run whose owner has since chosen to share reviews: adding
        // the first review publishes that review, and nothing else changes.
        let before = stored(Some(8), None, Private);
        let after = content(Some(9), Some("Loved it"));
        assert_eq!(
            resolve(RunChoice::default(), SHARE, Some(&before), &after),
            (Private, Private, Public)
        );
    }

    #[test]
    fn editing_existing_content_keeps_its_choice() {
        let before = stored(Some(8), Some("Draft"), Public);
        let after = content(Some(9), Some("Final"));
        assert_eq!(
            resolve(
                RunChoice::default(),
                SharingDefaults::PRIVATE,
                Some(&before),
                &after
            ),
            (Public, Public, Public)
        );
    }

    #[test]
    fn cleared_content_is_private_and_cannot_be_published_explicitly() {
        let before = stored(Some(8), Some("Gone"), Public);
        let after = content(None, None);
        assert_eq!(
            resolve(RunChoice::default(), SHARE, Some(&before), &after),
            (Public, Private, Private)
        );
        let error = RunChoice {
            rating: Some(Public),
            review: Some(Public),
            ..RunChoice::default()
        }
        .validate(&after)
        .expect_err("nothing to publish");
        let named: Vec<_> = error.fields.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(named, ["rating_visibility", "review_visibility"]);
    }
}
