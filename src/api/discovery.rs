//! Global discovery across every public profile: trending games, the Everyone
//! stream and people search. Each read is one statement over the gated public
//! sources. Today is the database's UTC date, used only to cap and window
//! chosen dates; write, edit and publication times are never read.
use super::*;
use sharing::PublicReview;
use social::{FeedCursor, FeedItem, FeedQuery, SocialPage};

/// Days, ending today, in which a completed run counts as finished recently.
const TRENDING_WINDOW_DAYS: i32 = 30;
/// Distinct active profiles a game needs before it is listed as trending.
const TRENDING_THRESHOLD: i64 = 3;
/// Reviews one author contributes to the Everyone stream per chosen day.
const EVERYONE_DAILY_CAP: i64 = 3;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/public/trending/games", get(trending_games))
        .route("/api/public/feed", get(everyone))
        .route("/api/public/people", get(people))
}

#[derive(Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct TrendingQuery {
    #[ts(optional)]
    pub limit: Option<u32>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct TrendingGames {
    pub data: Vec<TrendingGame>,
}

/// Counts of distinct public profiles only; never who they are.
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct TrendingGame {
    pub igdb_id: i64,
    pub game_name: String,
    pub game_slug: String,
    pub cover_url: Option<String>,
    /// Distinct profiles playing now or finished recently, counted once.
    pub active: i64,
    pub playing_now: i64,
    pub finished_recently: i64,
}

/// The same for every viewer. Blocks do not apply, because the response holds
/// counts and never people.
async fn trending_games(
    State(state): State<AppState>,
    query: Result<Query<TrendingQuery>, QueryRejection>,
) -> ApiResult<Json<TrendingGames>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let limit = query.limit.unwrap_or(10);
    if !(1..=50).contains(&limit) {
        return Err(ApiError::invalid("limit must be 1–50"));
    }
    // A completed run counts from D-29 to D+1: the day after today allows for
    // time zones ahead of UTC. Undated runs never count.
    let data = sqlx::query_as(
        "WITH today AS (SELECT (now() AT TIME ZONE 'UTC')::date AS d),
         playing AS (
           SELECT DISTINCT user_id,game_id FROM public_library_entries WHERE status='playing'
         ), finished AS (
           SELECT DISTINCT user_id,game_id FROM public_playthroughs,today
           WHERE state='completed' AND presentation_on BETWEEN d-($1::int-1) AND d+1
         ), people AS (
           SELECT user_id,game_id,bool_or(p) AS p,bool_or(f) AS f FROM (
             SELECT user_id,game_id,true AS p,false AS f FROM playing
             UNION ALL SELECT user_id,game_id,false,true FROM finished) x
           GROUP BY user_id,game_id
         )
         SELECT g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
                count(*) AS active,count(*) FILTER (WHERE p) AS playing_now,
                count(*) FILTER (WHERE f) AS finished_recently
         FROM people JOIN games g ON g.id=people.game_id
         GROUP BY g.id HAVING count(*)>=$2
         ORDER BY active DESC,playing_now DESC,g.igdb_id LIMIT $3",
    )
    .bind(TRENDING_WINDOW_DAYS)
    .bind(TRENDING_THRESHOLD)
    .bind(i64::from(limit))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(TrendingGames { data }))
}

/// Public reviews from every profile, newest chosen day first. An author
/// contributes at most three per chosen day; the rest stay on their profile.
async fn everyone(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    query: Result<Query<FeedQuery>, QueryRejection>,
) -> ApiResult<Json<SocialPage<FeedItem>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let limit = social::limit(query.limit)?;
    let cursor = query.after.as_deref().map(FeedCursor::parse).transpose()?;
    // The stream has no undated section and only one kind.
    let boundary = match cursor {
        Some(FeedCursor {
            date: Some(date),
            id,
            kind: 0,
        }) => Some((date, id)),
        Some(_) => return Err(ApiError::invalid("Invalid feed cursor")),
        None => None,
    };
    // Whole days are restricted before ranking and the (day, id) boundary is
    // applied after, so the cap keeps the same reviews on every page. The cap
    // also comes before the block filter; a block removes an author entirely.
    let mut rows: Vec<PublicReview> = sqlx::query_as(
        "WITH ranked AS (
           SELECT r.*,row_number() OVER (PARTITION BY user_id,presentation_on ORDER BY id DESC) AS n
           FROM public_reviews r
           WHERE presentation_on IS NOT NULL AND presentation_on<=(now() AT TIME ZONE 'UTC')::date+1
             AND ($1::date IS NULL OR presentation_on<=$1)
         )
         SELECT * FROM ranked
         WHERE n<=$3 AND public_view_allowed($4,user_id)
           AND ($1::date IS NULL OR presentation_on<$1 OR (presentation_on=$1 AND id<$2))
         ORDER BY presentation_on DESC,id DESC LIMIT $5",
    )
    .bind(boundary.map(|(date, _)| date))
    .bind(boundary.map(|(_, id)| id))
    .bind(EVERYONE_DAILY_CAP)
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
    .bind(i64::from(limit) + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(|review| {
            FeedCursor {
                date: review.presentation_on,
                id: review.id,
                kind: 0,
            }
            .encode()
        })
    } else {
        None
    };
    Ok(Json(SocialPage {
        data: rows.into_iter().map(FeedItem::Review).collect(),
        limit,
        has_more,
        next_cursor,
    }))
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct PeopleQuery {
    /// Required. One leading `@` is ignored.
    pub q: String,
    #[ts(optional)]
    pub limit: Option<u32>,
    #[ts(optional)]
    pub offset: Option<u32>,
}

/// A findable profile. No bio, counts or follow state.
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PersonSummary {
    /// The public profile UUID.
    pub id: Uuid,
    pub handle: Option<String>,
    pub display_name: String,
}

/// Enabled, unrestricted profiles whose handle begins with the term or whose
/// display name contains it. There is no total, to keep a count off the path.
async fn people(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    query: Result<Query<PeopleQuery>, QueryRejection>,
) -> ApiResult<Json<Page<PersonSummary>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = (query.limit.unwrap_or(20), query.offset.unwrap_or(0));
    if !(1..=50).contains(&limit) || offset > 10_000 {
        return Err(ApiError::invalid(
            "limit must be 1–50 and offset at most 10000",
        ));
    }
    let term = query.q.trim();
    let term = validate_search(term.strip_prefix('@').unwrap_or(term))?;
    let escaped = browsing::literal(term);
    let data = sqlx::query_as(
        "SELECT public_id AS id,handle,display_name FROM profiles
         WHERE public_enabled AND NOT moderation_hidden('profile',public_id)
           AND public_view_allowed($1,user_id)
           AND (handle LIKE lower($2) OR lower(display_name) LIKE lower($3))
         ORDER BY CASE WHEN handle=lower($4) THEN 0 WHEN handle LIKE lower($2) THEN 1 ELSE 2 END,
                  lower(display_name),public_id
         LIMIT $5 OFFSET $6",
    )
    .bind(viewer.0.map(|u| u.id.0.as_bytes().to_vec()))
    .bind(format!("{escaped}%"))
    .bind(format!("%{escaped}%"))
    .bind(term)
    .bind(i64::from(limit) + 1)
    .bind(i64::from(offset))
    .fetch_all(&state.pool)
    .await?;
    Ok(page(data, limit, offset))
}
