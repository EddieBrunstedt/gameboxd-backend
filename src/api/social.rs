//! Private follow relationships and live public-component feed reads. Future
//! blocking must constrain relationship writes and this shared source scope.
use super::*;
use sharing::{PublicGame, PublicNote, PublicPlaythrough, PublicReview};
use sqlx::{Postgres, QueryBuilder};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/following", get(following))
        .route(
            "/api/me/following/{id}",
            axum::routing::put(follow).delete(unfollow),
        )
        .route("/api/me/feed", get(feed))
}

#[derive(Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct FollowingQuery {
    #[ts(optional)]
    pub limit: Option<u32>,
    #[ts(optional)]
    pub after: Option<Uuid>,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct FollowedProfile {
    pub public_id: Uuid,
    pub display_name: String,
    pub handle: Option<String>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct SocialPage<T> {
    pub data: Vec<T>,
    pub limit: u32,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

fn limit(value: Option<u32>) -> ApiResult<u32> {
    let value = value.unwrap_or(20);
    if !(1..=50).contains(&value) {
        return Err(ApiError::invalid("limit must be 1–50"));
    }
    Ok(value)
}

async fn following(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<FollowingQuery>, QueryRejection>,
) -> ApiResult<Json<SocialPage<FollowedProfile>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let limit = limit(query.limit)?;
    // One statement provides a consistent authorization and pagination snapshot.
    let mut data: Vec<FollowedProfile> = sqlx::query_as(
        "SELECT p.public_id,p.display_name,p.handle FROM follows f
         JOIN profiles p ON p.user_id=f.followed_id AND p.public_enabled
         WHERE f.follower_id=$1 AND ($2::uuid IS NULL OR p.public_id>$2)
         ORDER BY p.public_id LIMIT $3",
    )
    .bind(user.0.id.0.as_bytes().as_slice())
    .bind(query.after)
    .bind(i64::from(limit) + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);
    let next_cursor = if has_more {
        data.last().map(|p| p.public_id.to_string())
    } else {
        None
    };
    Ok(Json(SocialPage {
        data,
        limit,
        has_more,
        next_cursor,
    }))
}

async fn set_follow(
    state: &AppState,
    owner: &[u8],
    id: Uuid,
    enabled: bool,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    // All operations on a target serialize with one another and with its
    // publication/deletion. In particular, a racing unfollow cannot miss an
    // uncommitted follow insertion. The FK handles account-deletion cascades.
    let target: Option<(Vec<u8>, bool)> =
        sqlx::query_as("SELECT user_id,public_enabled FROM profiles WHERE public_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((target, public)) = target {
        if enabled {
            if !public {
                return Err(ApiError::not_found());
            }
            if target == owner {
                return Err(ApiError::invalid("You cannot follow yourself"));
            }
            sqlx::query(
                "INSERT INTO follows(follower_id,followed_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
            )
            .bind(owner)
            .bind(&target)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query("DELETE FROM follows WHERE follower_id=$1 AND followed_id=$2")
                .bind(owner)
                .bind(&target)
                .execute(&mut *tx)
                .await?;
        }
    } else if enabled {
        return Err(ApiError::not_found());
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn follow(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<StatusCode> {
    set_follow(&state, user.0.id.0.as_bytes(), path(id)?, true).await
}

async fn unfollow(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<StatusCode> {
    set_follow(&state, user.0.id.0.as_bytes(), path(id)?, false).await
}

#[derive(Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct FeedQuery {
    #[ts(optional)]
    pub limit: Option<u32>,
    #[ts(optional)]
    pub after: Option<String>,
}

/// Identity is (kind, item.id). A milestone is the selected current summary,
/// never an audit event or a second copy of its independently shared review.
#[derive(Serialize, TS)]
#[serde(tag = "kind", content = "item", rename_all = "snake_case")]
#[ts(export)]
pub enum FeedItem {
    Review(PublicReview),
    Note(PublicNote),
    Milestone(PublicPlaythrough),
}

struct FeedCursor {
    date: Option<NaiveDate>,
    id: Uuid,
    kind: i16,
}

impl FeedCursor {
    fn parse(value: &str) -> ApiResult<Self> {
        let invalid = || ApiError::invalid("Invalid feed cursor");
        if value.len() > 90 {
            return Err(invalid());
        }
        let mut parts = value.split(':');
        if parts.next() != Some("f1") {
            return Err(invalid());
        }
        let date = parts.next().ok_or_else(invalid)?;
        let id = parts
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let kind = match parts.next() {
            Some("review") => 0,
            Some("note") => 1,
            Some("milestone") => 2,
            _ => return Err(invalid()),
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            date: if date == "unknown" {
                None
            } else {
                Some(date.parse().map_err(|_| invalid())?)
            },
            id,
            kind,
        })
    }

    fn encode(&self) -> String {
        let kind = match self.kind {
            0 => "review",
            1 => "note",
            _ => "milestone",
        };
        format!(
            "f1:{}:{}:{kind}",
            self.date
                .map_or_else(|| "unknown".into(), |d| d.to_string()),
            self.id
        )
    }

    fn filter(&self, sql: &mut QueryBuilder<Postgres>) {
        if let Some(date) = self.date {
            sql.push(" AND (presentation_on IS NULL OR presentation_on<")
                .push_bind(date)
                .push(" OR (presentation_on=")
                .push_bind(date)
                .push(" AND ");
        } else {
            sql.push(" AND presentation_on IS NULL AND ");
        }
        sql.push("(id,kind)<(")
            .push_bind(self.id)
            .push(",")
            .push_bind(self.kind)
            .push(")");
        if self.date.is_some() {
            sql.push("))");
        }
    }
}

#[derive(FromRow)]
struct FeedRow {
    id: Uuid,
    kind: i16,
    author_name: String,
    author_id: Uuid,
    #[sqlx(flatten)]
    game: PublicGame,
    presentation_on: Option<NaiveDate>,
    text: String,
    state: Option<PlaythroughState>,
    started_on: Option<NaiveDate>,
    finished_on: Option<NaiveDate>,
    platform: Option<String>,
}

impl FeedRow {
    fn cursor(&self) -> String {
        FeedCursor {
            date: self.presentation_on,
            id: self.id,
            kind: self.kind,
        }
        .encode()
    }
    fn into_item(self) -> FeedItem {
        match self.kind {
            0 => FeedItem::Review(PublicReview {
                id: self.id,
                author_name: self.author_name,
                author_id: self.author_id,
                game: self.game,
                presentation_on: self.presentation_on,
                review: self.text,
            }),
            1 => FeedItem::Note(PublicNote {
                id: self.id,
                author_name: self.author_name,
                author_id: self.author_id,
                game: self.game,
                presentation_on: self.presentation_on,
                text: self.text,
            }),
            _ => FeedItem::Milestone(PublicPlaythrough {
                id: self.id,
                author_name: self.author_name,
                author_id: self.author_id,
                game: self.game,
                presentation_on: self.presentation_on,
                state: self.state,
                started_on: self.started_on,
                finished_on: self.finished_on,
                platform: self.platform,
            }),
        }
    }
}

async fn feed(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<FeedQuery>, QueryRejection>,
) -> ApiResult<Json<SocialPage<FeedItem>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let limit = limit(query.limit)?;
    let cursor = query.after.as_deref().map(FeedCursor::parse).transpose()?;
    // A single statement snapshots relationships, profile gates, item choices,
    // content and the extra pagination row. No precomputed events survive a
    // revocation. Each arm starts from a public-safe, gated component view.
    let mut sql = QueryBuilder::<Postgres>::new(
        "WITH sources AS (SELECT followed_id FROM follows WHERE follower_id=",
    );
    sql.push_bind(user.0.id.0.as_bytes().to_vec()).push(
        "), items AS (
         SELECT id,0::smallint AS kind,author_name,author_id,igdb_id,game_name,game_slug,cover_url,
                presentation_on,review AS text,NULL::text AS state,NULL::date AS started_on,
                NULL::date AS finished_on,NULL::text AS platform
         FROM public_reviews WHERE user_id IN (SELECT followed_id FROM sources)
         UNION ALL
         SELECT id,1::smallint,author_name,author_id,igdb_id,game_name,game_slug,cover_url,
                presentation_on,text,NULL::text,NULL::date,NULL::date,NULL::text
         FROM public_notes WHERE user_id IN (SELECT followed_id FROM sources)
         UNION ALL
         SELECT id,2::smallint,author_name,author_id,igdb_id,game_name,game_slug,cover_url,
                presentation_on,''::text,state,started_on,finished_on,platform
         FROM public_playthroughs WHERE state IS NOT NULL AND user_id IN (SELECT followed_id FROM sources)
         ) SELECT * FROM items WHERE true",
    );
    if let Some(cursor) = cursor {
        cursor.filter(&mut sql);
    }
    let mut rows: Vec<FeedRow> = sql
        .push(" ORDER BY presentation_on DESC NULLS LAST,id DESC,kind DESC LIMIT ")
        .push_bind(i64::from(limit) + 1)
        .build_query_as()
        .fetch_all(&state.pool)
        .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next_cursor = if has_more {
        rows.last().map(FeedRow::cursor)
    } else {
        None
    };
    Ok(Json(SocialPage {
        data: rows.into_iter().map(FeedRow::into_item).collect(),
        limit,
        has_more,
        next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_preserve_unknown_dates_and_component_identity() {
        for date in [None, Some(NaiveDate::from_ymd_opt(2020, 2, 29).unwrap())] {
            for kind in 0..=2 {
                let cursor = FeedCursor {
                    date,
                    id: Uuid::new_v4(),
                    kind,
                };
                let parsed = FeedCursor::parse(&cursor.encode()).unwrap();
                assert_eq!(
                    (parsed.date, parsed.id, parsed.kind),
                    (cursor.date, cursor.id, cursor.kind)
                );
            }
        }
        for value in [
            "",
            "f1:unknown:bad:review",
            "f1:2020-02-30:00000000-0000-0000-0000-000000000000:note",
            "f1:unknown:00000000-0000-0000-0000-000000000000:rating",
            "f1:unknown:00000000-0000-0000-0000-000000000000:note:extra",
        ] {
            assert!(FeedCursor::parse(value).is_err(), "{value}");
        }
    }
}
