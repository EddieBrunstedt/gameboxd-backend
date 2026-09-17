//! Live aggregates from an authorized snapshot. Public inputs are selected
//! components, never owner aggregates with values hidden after calculation.
use super::*;
use crate::models::Visibility;
use sqlx::{PgConnection, Postgres, QueryBuilder};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/statistics", get(personal))
        .route("/api/public/profiles/{id}/statistics", get(public))
        .route("/api/me/public-preview/statistics", get(preview))
}

#[derive(Debug, Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct StatisticsQuery {
    #[ts(optional)]
    pub year: Option<i32>,
    #[ts(optional)]
    pub from_on: Option<NaiveDate>,
    #[ts(optional)]
    pub to_on: Option<NaiveDate>,
    #[ts(optional)]
    pub undated: Option<bool>,
}

impl StatisticsQuery {
    fn parse(value: Result<Query<Self>, QueryRejection>) -> ApiResult<Self> {
        let Query(value) = value.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
        value.browse().validate(Kind::Playthroughs)?;
        Ok(value)
    }

    fn browse(&self) -> BrowseQuery {
        BrowseQuery {
            year: self.year,
            from_on: self.from_on,
            to_on: self.to_on,
            undated: self.undated,
            ..Default::default()
        }
    }
}

/// Deliberate aggregate projection for both personal and public statistics.
/// Contains no owner records, identities, timestamps, or revision validators.
#[derive(Serialize, TS)]
#[ts(export)]
pub struct Statistics {
    pub playthroughs: PlaythroughStatistics,
    pub rating_distribution: Vec<RatingBucket>,
    // Always the current snapshot, unaffected by historical date filters.
    pub current_status_counts: browsing::StatusCounts,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct PlaythroughStatistics {
    pub played_playthroughs: i64,
    pub unique_games_played: i64,
    pub completed_playthroughs: i64,
    pub unique_completed_games: i64,
    pub replay_count: i64,
    pub replayed_games: i64,
    pub undated_playthroughs: i64,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct RatingBucket {
    pub rating: i16,
    pub count: i64,
}

// Only these fixed sources can reach the query builder. In particular, public
// ratings do not depend on summary publication, and summaries cannot reveal a
// private rating. Lists, reviews and notes never manufacture a recorded run.
#[derive(Clone, Copy)]
enum Inputs {
    Personal,
    Selected,
}

async fn calculate(
    conn: &mut PgConnection,
    owner: &[u8],
    inputs: Inputs,
    query: &StatisticsQuery,
) -> ApiResult<Statistics> {
    let (runs, ratings, library) = match inputs {
        Inputs::Personal => ("playthroughs", "playthroughs", "library_entries"),
        Inputs::Selected => (
            "selected_playthroughs",
            "selected_ratings",
            "selected_library_entries",
        ),
    };
    let filtered = |source: &'static str| {
        let mut sql = QueryBuilder::<Postgres>::new("WITH filtered AS (SELECT * FROM ");
        sql.push(source)
            .push(" WHERE user_id=")
            .push_bind(owner.to_vec());
        query.browse().filters(&mut sql, false);
        sql
    };
    let playthroughs = filtered(runs)
        .push(
            " AND (state IS NOT NULL OR started_on IS NOT NULL OR finished_on IS NOT NULL)),
            per_game AS (
                SELECT game_id,count(*) AS runs,
                    count(*) FILTER (WHERE state='completed') AS completions,
                    count(*) FILTER (WHERE presentation_on IS NULL) AS undated
                FROM filtered GROUP BY game_id
            ) SELECT
                COALESCE(sum(runs),0)::bigint AS played_playthroughs,
                count(*) AS unique_games_played,
                COALESCE(sum(completions),0)::bigint AS completed_playthroughs,
                count(*) FILTER (WHERE completions>0) AS unique_completed_games,
                COALESCE(sum(runs-1),0)::bigint AS replay_count,
                count(*) FILTER (WHERE runs>1) AS replayed_games,
                COALESCE(sum(undated),0)::bigint AS undated_playthroughs FROM per_game",
        )
        .build_query_as()
        .fetch_one(&mut *conn)
        .await?;
    let rating_distribution = filtered(ratings)
        .push(
            " AND rating IS NOT NULL)
            SELECT bucket::smallint AS rating,count(f.rating) AS count
            FROM generate_series(1,10) bucket
            LEFT JOIN filtered f ON f.rating=bucket
            GROUP BY bucket ORDER BY bucket",
        )
        .build_query_as()
        .fetch_all(&mut *conn)
        .await?;
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT count(*) FILTER (WHERE status='backlog') AS backlog,
            count(*) FILTER (WHERE status='playing') AS playing,
            count(*) FILTER (WHERE status='completed') AS completed,
            count(*) FILTER (WHERE status='on_hold') AS on_hold,
            count(*) FILTER (WHERE status='dropped') AS dropped FROM ",
    );
    let current_status_counts = sql
        .push(library)
        .push(" WHERE user_id=")
        .push_bind(owner)
        .build_query_as()
        .fetch_one(&mut *conn)
        .await?;
    Ok(Statistics {
        playthroughs,
        rating_distribution,
        current_status_counts,
    })
}

async fn personal(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<StatisticsQuery>, QueryRejection>,
) -> ApiResult<Json<Statistics>> {
    let query = StatisticsQuery::parse(query)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    let statistics = calculate(&mut tx, user.0.id.0.as_bytes(), Inputs::Personal, &query).await?;
    tx.commit().await?;
    Ok(Json(statistics))
}

async fn selected(
    state: &AppState,
    scope: profiles::Scope,
    query: StatisticsQuery,
) -> ApiResult<Json<Statistics>> {
    let mut tx = browsing::snapshot(&state.pool).await?;
    let profile = profiles::resolve(&mut tx, scope).await?;
    if profile.statistics_visibility != Visibility::Public {
        return Err(ApiError::not_found());
    }
    let statistics = calculate(&mut tx, &profile.user_id, Inputs::Selected, &query).await?;
    tx.commit().await?;
    Ok(Json(statistics))
}

async fn public(
    viewer: OptionalAuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<StatisticsQuery>, QueryRejection>,
) -> ApiResult<Json<Statistics>> {
    selected(
        &state,
        profiles::Scope::PublicId(path(id)?, viewer.0.map(|u| u.id.0.as_bytes().to_vec())),
        StatisticsQuery::parse(query)?,
    )
    .await
}

async fn preview(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<StatisticsQuery>, QueryRejection>,
) -> ApiResult<Json<Statistics>> {
    selected(
        &state,
        profiles::Scope::OwnerPreview(user.0.id.0.as_bytes().to_vec()),
        StatisticsQuery::parse(query)?,
    )
    .await
}
