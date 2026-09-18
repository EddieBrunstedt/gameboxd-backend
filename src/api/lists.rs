//! A list is one revisioned aggregate: metadata, permission and ordered members.
//! All mutations lock its parent row before inspecting or changing membership.
use super::*;
use crate::models::Visibility;
use chrono::{DateTime, Utc};
use sharing::{PublicGame, VisibilityInput};
use sqlx::{PgConnection, Postgres, Transaction};
use std::collections::HashSet;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/lists", get(index).post(create))
        .route("/api/me/lists/{id}", get(read).put(replace).delete(remove))
        .route("/api/me/lists/{id}/games", axum::routing::put(set_games))
        .route(
            "/api/me/lists/{id}/visibility",
            axum::routing::put(set_visibility),
        )
        .route("/api/public/lists/{id}", get(public_read))
        .route("/api/me/public-preview/lists/{id}", get(preview_read))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[ts(export)]
pub enum ListKind {
    #[default]
    Custom,
    Favorites,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct GameList {
    pub id: Uuid,
    pub kind: ListKind,
    pub title: String,
    pub description: String,
    pub visibility: Visibility,
    pub game_count: i64,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct GameListDetail {
    pub list: GameList,
    /// Array order is the owner's chosen order; all members are returned.
    pub games: Vec<PublicGame>,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PublicList {
    pub id: Uuid,
    pub kind: ListKind,
    pub title: String,
    pub description: String,
    pub game_count: i64,
    pub author_id: Uuid,
    pub author_name: String,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct PublicListDetail {
    pub list: PublicList,
    pub games: Vec<PublicGame>,
}

#[derive(Deserialize, TS, Serialize)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct CreateListInput {
    #[serde(default)]
    #[ts(optional, as = "Option<ListKind>")]
    pub kind: ListKind,
    pub title: String,
    #[serde(default)]
    #[ts(optional, as = "Option<String>")]
    pub description: String,
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ListInput {
    pub title: String,
    #[serde(default)]
    #[ts(optional, as = "Option<String>")]
    pub description: String,
}

impl ListInput {
    fn validated(self) -> ApiResult<Self> {
        let mut errors = Vec::new();
        for (field, text, max) in [
            ("title", &self.title, 120),
            ("description", &self.description, 5000),
        ] {
            let error = if field == "title" && text.trim().is_empty() {
                Some((
                    FieldErrorCode::Required,
                    "title must not be blank".to_owned(),
                ))
            } else if text.chars().count() > max {
                Some((
                    FieldErrorCode::TooLong,
                    format!("{field} is limited to {max} characters"),
                ))
            } else if text.contains('\0') {
                Some((
                    FieldErrorCode::InvalidFormat,
                    format!("{field} may not contain NUL"),
                ))
            } else {
                None
            };
            if let Some((code, message)) = error {
                errors.push(FieldError::new(field, code, message));
            }
        }
        if !errors.is_empty() {
            return Err(ApiError::fields(errors));
        }
        Ok(self)
    }
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ListGamesInput {
    /// Complete replacement in the desired order. No duplicate IDs; at most 500.
    #[ts(type = "Array<number>")]
    pub igdb_ids: Vec<i64>,
}

impl ListGamesInput {
    fn validate(&self) -> ApiResult<()> {
        let unique: HashSet<_> = self.igdb_ids.iter().collect();
        if self.igdb_ids.len() > 500
            || unique.len() != self.igdb_ids.len()
            || self.igdb_ids.iter().any(|id| *id <= 0)
        {
            return Err(ApiError::fields(vec![FieldError::new(
                "igdb_ids",
                FieldErrorCode::InvalidFormat,
                "Use at most 500 distinct positive IGDB IDs in the desired order",
            )]));
        }
        Ok(())
    }
}

struct TaggedList(GameListDetail);
impl IntoResponse for TaggedList {
    fn into_response(self) -> Response {
        (
            [(header::ETAG, format!("\"{}\"", self.0.list.version))],
            Json(self.0),
        )
            .into_response()
    }
}

async fn members(conn: &mut PgConnection, id: Uuid) -> ApiResult<Vec<PublicGame>> {
    Ok(sqlx::query_as("SELECT g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url
        FROM game_list_members m JOIN games g ON g.id=m.game_id WHERE m.list_id=$1 ORDER BY m.position")
        .bind(id).fetch_all(conn).await?)
}

async fn saved(conn: &mut PgConnection, owner: &[u8], id: Uuid) -> ApiResult<GameListDetail> {
    let list = sqlx::query_as("SELECT * FROM game_list_details WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(GameListDetail {
        list,
        games: members(conn, id).await?,
    })
}

// A replacement must name a revision the caller saw. Wildcard writes would
// allow a stale reorder to erase a concurrently added game.
fn list_precondition(headers: &HeaderMap) -> ApiResult<Precondition> {
    let precondition = required_precondition(headers)?;
    if precondition.accepts_any() {
        return Err(ApiError::invalid(
            "List edits require a revision ETag; If-Match: * is not supported",
        ));
    }
    Ok(precondition)
}

async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &[u8],
    id: Uuid,
    precondition: &Precondition,
) -> ApiResult<()> {
    let version =
        sqlx::query_scalar("SELECT version FROM game_lists WHERE user_id=$1 AND id=$2 FOR UPDATE")
            .bind(owner)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    if !precondition.is_met_by(version) {
        return Err(stale());
    }
    Ok(())
}

pub(super) const SELECTED: &str = "SELECT *,title AS game_name FROM selected_lists";

async fn index(
    user: AuthUser,
    State(state): State<AppState>,
    params: Result<Query<BrowseQuery>, QueryRejection>,
) -> ApiResult<Json<BrowsePage<GameList>>> {
    let params = BrowseQuery::parse(params)?;
    params.validate(Kind::Lists)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    let page = browsing::read_page(
        &mut tx,
        user.0.id.0.as_bytes().as_slice(),
        "SELECT *,title AS game_name FROM game_list_details",
        &params,
        Kind::Lists,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(page))
}

async fn read(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<TaggedList> {
    let mut tx = browsing::snapshot(&state.pool).await?;
    let row = saved(&mut tx, user.0.id.0.as_bytes().as_slice(), path(id)?).await?;
    tx.commit().await?;
    Ok(TaggedList(row))
}

async fn create(
    user: AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Body<CreateListInput>, ApiError>,
) -> ApiResult<Response> {
    let input = body(payload)?;
    let receipt = retry::Receipt::new(&headers, "list:create", &input)?;
    let metadata = ListInput {
        title: input.title,
        description: input.description,
    }
    .validated()?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    if let Some(response) = receipt.replay(&mut tx, user.0.id.0.as_bytes()).await? {
        return Ok(response);
    }
    let inserted: Option<Uuid> = sqlx::query_scalar("INSERT INTO game_lists (id,user_id,kind,title,description)
        VALUES ($1,$2,$3,$4,$5) ON CONFLICT (user_id) WHERE kind='favorites' DO NOTHING RETURNING id")
        .bind(id).bind(owner).bind(input.kind).bind(metadata.title).bind(metadata.description)
        .fetch_optional(&mut *tx).await?;
    if inserted.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::FavoritesExists,
            "A favorites list already exists",
        ));
    }
    let row = saved(&mut tx, owner, id).await?;
    receipt
        .finish(
            tx,
            user.0.id.0.as_bytes(),
            (StatusCode::CREATED, TaggedList(row)),
        )
        .await
}

async fn replace(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<ListInput>, ApiError>,
) -> ApiResult<TaggedList> {
    let id = path(id)?;
    let precondition = list_precondition(&headers)?;
    let input = body(payload)?.validated()?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock(&mut tx, owner, id, &precondition).await?;
    sqlx::query("UPDATE game_lists SET title=$3,description=$4 WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .bind(input.title)
        .bind(input.description)
        .execute(&mut *tx)
        .await?;
    let row = saved(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(TaggedList(row))
}

async fn set_games(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<ListGamesInput>, ApiError>,
) -> ApiResult<TaggedList> {
    let id = path(id)?;
    let precondition = list_precondition(&headers)?;
    let input = body(payload)?;
    input.validate()?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock(&mut tx, owner, id, &precondition).await?;
    // Local catalog only: import first. Never hold a list lock across IGDB I/O.
    let games: Vec<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM unnest($1::bigint[]) WITH ORDINALITY i(igdb_id,position)
        JOIN games g ON g.igdb_id=i.igdb_id ORDER BY i.position",
    )
    .bind(&input.igdb_ids)
    .fetch_all(&mut *tx)
    .await?;
    if games.len() != input.igdb_ids.len() {
        return Err(ApiError::fields(vec![FieldError::new(
            "igdb_ids",
            FieldErrorCode::InvalidFormat,
            "Import every game into the catalog before adding it to a list",
        )]));
    }
    let current: Vec<Uuid> = sqlx::query_scalar(
        "SELECT game_id FROM game_list_members WHERE list_id=$1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    if current != games {
        sqlx::query("DELETE FROM game_list_members WHERE list_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO game_list_members (list_id,game_id,position)
            SELECT $1,game_id,(position-1)::integer FROM unnest($2::uuid[]) WITH ORDINALITY i(game_id,position)")
            .bind(id).bind(&games).execute(&mut *tx).await?;
        sqlx::query("UPDATE game_lists SET version=version+1 WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    let row = saved(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(TaggedList(row))
}

async fn set_visibility(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Body<VisibilityInput>, ApiError>,
) -> ApiResult<TaggedList> {
    let id = path(id)?;
    let precondition = list_precondition(&headers)?;
    let input = body(payload)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock(&mut tx, owner, id, &precondition).await?;
    sqlx::query("UPDATE game_lists SET visibility=$3 WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .bind(input.visibility)
        .execute(&mut *tx)
        .await?;
    let row = saved(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(TaggedList(row))
}

async fn remove(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let id = path(id)?;
    let precondition = list_precondition(&headers)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    lock(&mut tx, owner, id, &precondition).await?;
    sqlx::query("DELETE FROM game_lists WHERE user_id=$1 AND id=$2")
        .bind(owner)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn selected_detail(
    state: &AppState,
    id: Uuid,
    preview_owner: Option<&[u8]>,
    viewer: Option<Vec<u8>>,
) -> ApiResult<Json<PublicListDetail>> {
    let mut tx = browsing::snapshot(&state.pool).await?;
    // The optional owner comes only from AuthUser, never visitor input.
    let list = if let Some(owner) = preview_owner {
        sqlx::query_as("SELECT * FROM selected_lists WHERE id=$1 AND user_id=$2")
            .bind(id)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        sqlx::query_as("SELECT * FROM public_lists WHERE id=$1 AND public_view_allowed($2,user_id)")
            .bind(id)
            .bind(viewer)
            .fetch_optional(&mut *tx)
            .await?
    }
    .ok_or_else(ApiError::not_found)?;
    let games = members(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(PublicListDetail { list, games }))
}

async fn public_read(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicListDetail>> {
    selected_detail(
        &state,
        path(id)?,
        None,
        viewer.0.map(|u| u.id.0.as_bytes().to_vec()),
    )
    .await
}

async fn preview_read(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<PublicListDetail>> {
    selected_detail(
        &state,
        path(id)?,
        Some(user.0.id.0.as_bytes().as_slice()),
        None,
    )
    .await
}
