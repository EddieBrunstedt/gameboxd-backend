//! Profile reads resolve authorization and project selected items in one snapshot.
//! Only the session-owned preview can bypass the profile gate, never item choices.
use super::*;
use browsing::snapshot;
use sharing::{PublicLibraryEntry, PublicNote, PublicPlaythrough, PublicRating, PublicReview};
use sqlx::PgConnection;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/handle", axum::routing::put(set_handle))
        .route("/api/me/public-preview", get(preview))
        .route("/api/me/public-preview/{section}", get(preview_section))
        .route("/api/public/profiles/{id}", get(profile))
        .route("/api/public/profiles/{id}/{section}", get(section))
        .route("/api/public/handles/{handle}", get(by_handle))
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct HandleInput {
    pub handle: String,
}

fn canonical_handle(value: &str) -> ApiResult<String> {
    let bytes = value.as_bytes();
    if !(3..=30).contains(&bytes.len())
        || !bytes[0].is_ascii_alphabetic()
        || !bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    {
        return Err(ApiError::fields(vec![FieldError::new(
            "handle",
            FieldErrorCode::InvalidFormat,
            "Use 3–30 ASCII letters, digits or underscores, starting with a letter",
        )]));
    }
    Ok(value.to_ascii_lowercase())
}

async fn set_handle(
    user: AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Body<HandleInput>, ApiError>,
) -> ApiResult<Json<Profile>> {
    let precondition = required_precondition(&headers)?;
    let handle = canonical_handle(&body(payload)?.handle)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    let current: Profile = sqlx::query_as("SELECT * FROM profiles WHERE user_id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(current.version) {
        return Err(stale());
    }
    // The PK arbitrates simultaneous claims, including on different profiles.
    // A retired name can only be restored by its original owner.
    let claimed: Option<String> = sqlx::query_scalar(
        "INSERT INTO profile_handle_reservations (handle,profile_id) VALUES ($1,$2)
         ON CONFLICT (handle) DO UPDATE SET profile_id=EXCLUDED.profile_id
         WHERE profile_handle_reservations.profile_id=EXCLUDED.profile_id RETURNING handle",
    )
    .bind(&handle)
    .bind(current.public_id)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::HandleUnavailable,
            "Handle is unavailable",
        ));
    }
    let saved = sqlx::query_as("UPDATE profiles SET handle=$2 WHERE user_id=$1 RETURNING *")
        .bind(owner)
        .bind(handle)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(saved))
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicProfileCounts {
    pub current_games: i64,
    pub library: i64,
    pub playthroughs: i64,
    pub ratings: i64,
    pub reviews: i64,
    pub notes: i64,
    pub lists: i64,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct PublicProfile {
    pub id: Uuid,
    pub handle: Option<String>,
    pub display_name: String,
    pub bio: Option<String>,
    pub counts: PublicProfileCounts,
}

#[derive(Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ProfileSection {
    CurrentGames,
    Library,
    Playthroughs,
    Ratings,
    Reviews,
    Notes,
    Lists,
}

#[derive(Serialize, TS)]
#[serde(tag = "section", content = "page", rename_all = "snake_case")]
#[ts(export)]
pub enum PublicProfileSection {
    CurrentGames(BrowsePage<PublicLibraryEntry>),
    Library(BrowsePage<PublicLibraryEntry>),
    Playthroughs(BrowsePage<PublicPlaythrough>),
    Ratings(BrowsePage<PublicRating>),
    Reviews(BrowsePage<PublicReview>),
    Notes(BrowsePage<PublicNote>),
    Lists(BrowsePage<lists::PublicList>),
}

// Scope is constructed by handlers, never deserialized from visitor input.
pub(super) enum Scope {
    PublicId(Uuid, Option<Vec<u8>>),
    Handle(String, Option<Vec<u8>>),
    OwnerPreview(Vec<u8>),
}

#[derive(FromRow)]
pub(super) struct SelectedProfile {
    pub(super) user_id: Vec<u8>,
    pub(super) statistics_visibility: crate::models::Visibility,
    id: Uuid,
    handle: Option<String>,
    display_name: String,
    bio: Option<String>,
}

pub(super) async fn resolve(conn: &mut PgConnection, scope: Scope) -> ApiResult<SelectedProfile> {
    let (id, handle, owner, viewer) = match scope {
        Scope::PublicId(id, viewer) => (Some(id), None, None, viewer),
        Scope::Handle(handle, viewer) => (None, Some(handle), None, viewer),
        Scope::OwnerPreview(owner) => (None, None, Some(owner), None),
    };
    sqlx::query_as(
        "SELECT user_id,statistics_visibility,public_id AS id,handle,display_name,
                CASE WHEN bio_visibility='public' THEN bio END AS bio
         FROM profiles WHERE public_view_allowed($4,user_id) AND NOT moderation_hidden('profile',public_id) AND ((public_enabled AND (public_id=$1 OR handle=$2)) OR user_id=$3)",
    )
    .bind(id)
    .bind(handle)
    .bind(owner)
    .bind(viewer)
    .fetch_optional(conn)
    .await?
    .ok_or_else(ApiError::not_found)
}

async fn read_profile(state: &AppState, scope: Scope) -> ApiResult<Json<PublicProfile>> {
    let mut tx = snapshot(&state.pool).await?;
    let profile = resolve(&mut tx, scope).await?;
    let counts = sqlx::query_as(
        "SELECT
        (SELECT count(*) FROM selected_library_entries WHERE user_id=$1 AND status='playing') AS current_games,
        (SELECT count(*) FROM selected_library_entries WHERE user_id=$1) AS library,
        (SELECT count(*) FROM selected_playthroughs WHERE user_id=$1) AS playthroughs,
        (SELECT count(*) FROM selected_ratings WHERE user_id=$1) AS ratings,
        (SELECT count(*) FROM selected_reviews WHERE user_id=$1) AS reviews,
        (SELECT count(*) FROM selected_notes WHERE user_id=$1) AS notes,
        (SELECT count(*) FROM selected_lists WHERE user_id=$1) AS lists",
    ).bind(&profile.user_id).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(PublicProfile {
        id: profile.id,
        handle: profile.handle,
        display_name: profile.display_name,
        bio: profile.bio,
        counts,
    }))
}

async fn selected_page<T>(
    conn: &mut PgConnection,
    owner: &[u8],
    section: &ProfileSection,
    params: &BrowseQuery,
) -> ApiResult<BrowsePage<T>>
where
    T: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    let (source, kind) = match section {
        ProfileSection::CurrentGames => (browsing::PUBLIC_CURRENT, Kind::Library { public: true }),
        ProfileSection::Library => (browsing::PUBLIC_LIBRARY, Kind::Library { public: true }),
        ProfileSection::Playthroughs => (browsing::PUBLIC_PLAYTHROUGHS, Kind::Playthroughs),
        ProfileSection::Ratings => ("SELECT * FROM selected_ratings", Kind::Ratings),
        ProfileSection::Reviews => (
            "SELECT *,review AS text FROM selected_reviews",
            Kind::Reviews,
        ),
        ProfileSection::Notes => ("SELECT * FROM selected_notes", Kind::PublicNotes),
        ProfileSection::Lists => (lists::SELECTED, Kind::Lists),
    };
    params.validate(kind)?;
    browsing::read_page(conn, owner, source, params, kind, None).await
}

async fn read_section(
    state: &AppState,
    scope: Scope,
    section: ProfileSection,
    params: BrowseQuery,
) -> ApiResult<Json<PublicProfileSection>> {
    let mut tx = snapshot(&state.pool).await?;
    let profile = resolve(&mut tx, scope).await?;
    let owner = &profile.user_id;
    let result = match section {
        ProfileSection::CurrentGames => PublicProfileSection::CurrentGames(
            selected_page(&mut tx, owner, &section, &params).await?,
        ),
        ProfileSection::Library => {
            PublicProfileSection::Library(selected_page(&mut tx, owner, &section, &params).await?)
        }
        ProfileSection::Playthroughs => PublicProfileSection::Playthroughs(
            selected_page(&mut tx, owner, &section, &params).await?,
        ),
        ProfileSection::Ratings => {
            PublicProfileSection::Ratings(selected_page(&mut tx, owner, &section, &params).await?)
        }
        ProfileSection::Reviews => {
            PublicProfileSection::Reviews(selected_page(&mut tx, owner, &section, &params).await?)
        }
        ProfileSection::Notes => {
            PublicProfileSection::Notes(selected_page(&mut tx, owner, &section, &params).await?)
        }
        ProfileSection::Lists => {
            PublicProfileSection::Lists(selected_page(&mut tx, owner, &section, &params).await?)
        }
    };
    tx.commit().await?;
    Ok(Json(result))
}

async fn profile(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicProfile>> {
    read_profile(
        &state,
        Scope::PublicId(path(id)?, viewer.0.map(|u| u.id.0.as_bytes().to_vec())),
    )
    .await
}

async fn by_handle(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    handle: Result<Path<String>, PathRejection>,
) -> ApiResult<Json<PublicProfile>> {
    let handle = canonical_handle(&path(handle)?).map_err(|_| ApiError::not_found())?;
    read_profile(
        &state,
        Scope::Handle(handle, viewer.0.map(|u| u.id.0.as_bytes().to_vec())),
    )
    .await
}

async fn preview(user: AuthUser, State(state): State<AppState>) -> ApiResult<Json<PublicProfile>> {
    read_profile(&state, Scope::OwnerPreview(user.0.id.0.as_bytes().to_vec())).await
}

async fn section(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<(Uuid, ProfileSection)>, PathRejection>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<PublicProfileSection>> {
    let (id, section) = path(id)?;
    read_section(
        &state,
        Scope::PublicId(id, viewer.0.map(|u| u.id.0.as_bytes().to_vec())),
        section,
        BrowseQuery::parse(params)?,
    )
    .await
}

async fn preview_section(
    user: AuthUser,
    State(state): State<AppState>,
    section: Result<Path<ProfileSection>, PathRejection>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<PublicProfileSection>> {
    read_section(
        &state,
        Scope::OwnerPreview(user.0.id.0.as_bytes().to_vec()),
        path(section)?,
        BrowseQuery::parse(params)?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_have_one_ascii_canonical_form() {
        assert_eq!(canonical_handle("Player_42").unwrap(), "player_42");
        assert!(canonical_handle(&"a".repeat(30)).is_ok());
        for invalid in [
            "",
            "ab",
            "1abc",
            "_abc",
            "a-b",
            "a.b",
            " abc",
            "abc ",
            "a/b",
            "åbc",
            "Ａbc",
            "a\0b",
            &"a".repeat(31),
        ] {
            assert!(canonical_handle(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
