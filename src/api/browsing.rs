//! Shared, snapshot-consistent browsing. Sources contain only fields the caller
//! may filter on; public sources must never bring private components into scope.
use super::*;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row, Transaction};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum LibrarySort {
    #[default]
    Title,
    Rating,
    DateAdded,
    RecentlyPlayed,
}

#[derive(Debug, Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct BrowseQuery {
    #[ts(optional)]
    pub limit: Option<u32>,
    #[ts(optional)]
    pub offset: Option<u32>,
    #[ts(optional)]
    pub after: Option<String>,
    #[ts(optional)]
    pub q: Option<String>,
    #[ts(optional)]
    pub igdb_id: Option<i64>,
    #[ts(optional)]
    pub year: Option<i32>,
    #[ts(optional)]
    pub from_on: Option<NaiveDate>,
    #[ts(optional)]
    pub to_on: Option<NaiveDate>,
    #[ts(optional)]
    pub undated: Option<bool>,
    #[ts(optional)]
    pub state: Option<PlaythroughState>,
    #[ts(optional)]
    pub has_rating: Option<bool>,
    #[ts(optional)]
    pub has_review: Option<bool>,
    #[ts(optional)]
    pub text: Option<String>,
    #[ts(optional)]
    pub status: Option<PlayStatus>,
    #[ts(optional)]
    pub sort: Option<LibrarySort>,
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Library { public: bool },
    Playthroughs,
    OwnerNotes,
    PublicNotes,
    Ratings,
    Reviews,
    Lists,
}

impl Kind {
    fn library(self) -> bool {
        matches!(self, Self::Library { .. })
    }
}

impl BrowseQuery {
    pub(super) fn parse(value: Result<Query<Self>, QueryRejection>) -> ApiResult<Self> {
        let Query(value) = value.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
        Ok(value)
    }

    pub(super) fn limit(&self) -> u32 {
        self.limit.unwrap_or(20)
    }
    fn offset(&self) -> u32 {
        self.offset.unwrap_or(0)
    }

    pub(super) fn validate(&self, kind: Kind) -> ApiResult<()> {
        if !(1..=50).contains(&self.limit()) || self.offset() > 10_000 {
            return Err(ApiError::invalid(
                "limit must be 1–50 and offset at most 10000",
            ));
        }
        if self.igdb_id.is_some_and(|id| id <= 0) {
            return Err(ApiError::invalid("IGDB ID must be positive"));
        }
        for term in [&self.q, &self.text].into_iter().flatten() {
            validate_search(term)?;
        }
        if self.year.is_some_and(|year| !(1..=9999).contains(&year))
            || self.from_on.zip(self.to_on).is_some_and(|(a, b)| a > b)
            || (self.year.is_some() && (self.from_on.is_some() || self.to_on.is_some()))
            || (self.undated == Some(true)
                && (self.year.is_some() || self.from_on.is_some() || self.to_on.is_some()))
        {
            return Err(ApiError::invalid(
                "Use year (1–9999) or an inclusive ordered date range; undated=true cannot have dates",
            ));
        }
        if let Some(after) = &self.after {
            Cursor::parse(after)?;
            if matches!(kind, Kind::Lists) || kind.library() || self.offset() != 0 {
                return Err(ApiError::invalid(
                    "after requires a dated collection and offset=0",
                ));
            }
        }
        let dates = self.year.is_some()
            || self.from_on.is_some()
            || self.to_on.is_some()
            || self.undated.is_some();
        let invalid = if matches!(kind, Kind::Lists) {
            dates
                || self.igdb_id.is_some()
                || self.state.is_some()
                || self.has_rating.is_some()
                || self.has_review.is_some()
                || self.text.is_some()
                || self.status.is_some()
                || self.sort.is_some()
        } else if kind.library() {
            dates
                || self.state.is_some()
                || self.has_review.is_some()
                || self.text.is_some()
                || (matches!(kind, Kind::Library { public: true })
                    && self.sort == Some(LibrarySort::DateAdded))
        } else {
            self.status.is_some()
                || self.sort.is_some()
                || (self.state.is_some() && !matches!(kind, Kind::Playthroughs | Kind::OwnerNotes))
                || ((self.has_rating.is_some() || self.has_review.is_some())
                    && !matches!(kind, Kind::Playthroughs | Kind::OwnerNotes))
                || (self.text.is_some()
                    && !matches!(kind, Kind::OwnerNotes | Kind::PublicNotes | Kind::Reviews))
        };
        if invalid {
            return Err(ApiError::invalid(
                "Filter or sort is not available for this collection",
            ));
        }
        Ok(())
    }

    pub(super) fn filters(&self, sql: &mut QueryBuilder<Postgres>, include_status: bool) {
        if let Some(q) = &self.q {
            sql.push(" AND lower(game_name) LIKE lower(")
                .push_bind(pattern(q))
                .push(")");
        }
        if let Some(id) = self.igdb_id {
            sql.push(" AND igdb_id=").push_bind(id);
        }
        let (from, to) = if let Some(year) = self.year {
            (
                NaiveDate::from_ymd_opt(year, 1, 1),
                NaiveDate::from_ymd_opt(year, 12, 31),
            )
        } else {
            (self.from_on, self.to_on)
        };
        if let Some(from) = from {
            sql.push(" AND presentation_on>=").push_bind(from);
        }
        if let Some(to) = to {
            sql.push(" AND presentation_on<=").push_bind(to);
        }
        if let Some(undated) = self.undated {
            sql.push(if undated {
                " AND presentation_on IS NULL"
            } else {
                " AND presentation_on IS NOT NULL"
            });
        }
        if let Some(state) = self.state {
            sql.push(" AND state=").push_bind(state);
        }
        if let Some(present) = self.has_rating {
            sql.push(" AND (rating IS NOT NULL)=").push_bind(present);
        }
        if let Some(present) = self.has_review {
            sql.push(" AND (review IS NOT NULL)=").push_bind(present);
        }
        if let Some(text) = &self.text {
            sql.push(" AND lower(text) LIKE lower(")
                .push_bind(pattern(text))
                .push(")");
        }
        if include_status && let Some(status) = self.status {
            sql.push(" AND status=").push_bind(status);
        }
    }
}

fn pattern(term: &str) -> String {
    format!("%{}%", literal(term))
}

/// A trimmed search term with LIKE's wildcards and escape character escaped,
/// so it matches literally inside a pattern.
pub(super) fn literal(term: &str) -> String {
    term.trim()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

struct Cursor {
    date: Option<NaiveDate>,
    id: Uuid,
}
impl Cursor {
    fn parse(value: &str) -> ApiResult<Self> {
        let invalid = || ApiError::invalid("Invalid diary cursor");
        if value.len() > 80 {
            return Err(invalid());
        }
        let (date, id) = value
            .strip_prefix("v1:")
            .and_then(|v| v.split_once(':'))
            .ok_or_else(invalid)?;
        Ok(Self {
            date: if date == "unknown" {
                None
            } else {
                Some(date.parse().map_err(|_| invalid())?)
            },
            id: id.parse().map_err(|_| invalid())?,
        })
    }
    fn encode(&self) -> String {
        format!(
            "v1:{}:{}",
            self.date
                .map_or_else(|| "unknown".into(), |date| date.to_string()),
            self.id
        )
    }
    fn filter(&self, sql: &mut QueryBuilder<Postgres>) {
        if let Some(date) = self.date {
            sql.push(" AND (presentation_on IS NULL OR presentation_on<")
                .push_bind(date)
                .push(" OR (presentation_on=")
                .push_bind(date)
                .push(" AND id<")
                .push_bind(self.id)
                .push("))");
        } else {
            sql.push(" AND presentation_on IS NULL AND id<")
                .push_bind(self.id);
        }
    }
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct StatusCounts {
    pub backlog: i64,
    pub playing: i64,
    pub completed: i64,
    pub on_hold: i64,
    pub dropped: i64,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct BrowsePage<T> {
    pub data: Vec<T>,
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
    pub total: i64,
    pub next_cursor: Option<String>,
    /// Library facets respect all filters except status, and ignore pagination.
    pub status_counts: Option<StatusCounts>,
}

pub(super) async fn snapshot(pool: &sqlx::PgPool) -> ApiResult<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Source SQL is a server constant, never visitor input. Every user value binds.
/// The caller authorizes the owner and validates supported filters first.
pub(super) async fn read_page<T>(
    conn: &mut PgConnection,
    owner: &[u8],
    source: &'static str,
    params: &BrowseQuery,
    kind: Kind,
    parent: Option<Uuid>,
) -> ApiResult<BrowsePage<T>>
where
    T: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    let base = |select: &'static str, status: bool| {
        let mut sql = QueryBuilder::<Postgres>::new(select);
        sql.push(" FROM (")
            .push(source)
            .push(") items WHERE user_id=")
            .push_bind(owner.to_vec());
        if let Some(parent) = parent {
            sql.push(" AND playthrough_id=").push_bind(parent);
        }
        params.filters(&mut sql, status);
        sql
    };
    let total: i64 = base("SELECT count(*)", true)
        .build_query_scalar()
        .fetch_one(&mut *conn)
        .await?;
    let status_counts = if kind.library() {
        Some(
            base(
                "SELECT count(*) FILTER (WHERE status='backlog') AS backlog,
            count(*) FILTER (WHERE status='playing') AS playing,
            count(*) FILTER (WHERE status='completed') AS completed,
            count(*) FILTER (WHERE status='on_hold') AS on_hold,
            count(*) FILTER (WHERE status='dropped') AS dropped",
                false,
            )
            .build_query_as()
            .fetch_one(&mut *conn)
            .await?,
        )
    } else {
        None
    };
    let mut sql = base("SELECT *", true);
    if let Some(after) = &params.after {
        Cursor::parse(after)?.filter(&mut sql);
    }
    let order = if matches!(kind, Kind::Lists) {
        "lower(title),id"
    } else if kind.library() {
        match params.sort.unwrap_or_default() {
            LibrarySort::Title => "lower(game_name),igdb_id",
            LibrarySort::Rating => "rating DESC NULLS LAST,lower(game_name),igdb_id",
            LibrarySort::DateAdded => "created_at DESC,lower(game_name),igdb_id",
            LibrarySort::RecentlyPlayed => {
                "last_played_on DESC NULLS LAST,lower(game_name),igdb_id"
            }
        }
    } else {
        "presentation_on DESC NULLS LAST,id DESC"
    };
    sql.push(" ORDER BY ")
        .push(order)
        .push(" LIMIT ")
        .push_bind(i64::from(params.limit()) + 1)
        .push(" OFFSET ")
        .push_bind(i64::from(params.offset()));
    let mut rows = sql.build().fetch_all(&mut *conn).await?;
    let has_more = rows.len() > params.limit() as usize;
    rows.truncate(params.limit() as usize);
    let next_cursor = if has_more && !kind.library() && !matches!(kind, Kind::Lists) {
        rows.last()
            .map(|row| {
                Ok::<_, sqlx::Error>(
                    Cursor {
                        date: row.try_get("presentation_on")?,
                        id: row.try_get("id")?,
                    }
                    .encode(),
                )
            })
            .transpose()?
    } else {
        None
    };
    let data = rows.iter().map(T::from_row).collect::<Result<_, _>>()?;
    Ok(BrowsePage {
        data,
        limit: params.limit(),
        offset: params.offset(),
        has_more,
        total,
        next_cursor,
        status_counts,
    })
}

pub(super) const OWNER_PLAYTHROUGHS: &str = "SELECT p.*,g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url FROM playthroughs p JOIN games g ON g.id=p.game_id";
pub(super) const OWNER_NOTES: &str = "SELECT n.*,p.state,p.rating,p.review FROM diary_note_details n JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id";
pub(super) const OWNER_LIBRARY: &str = "SELECT g.id,g.igdb_id,g.name,g.name AS game_name,g.slug,g.summary,g.cover_url,g.first_release_date,
    g.platforms,g.genres,g.developers,g.game_type,g.parent_igdb_id,g.version_parent_igdb_id,g.source_url,g.imported_at,g.refreshed_at,
    e.user_id,e.status,e.rating,e.rating_playthrough_id,e.current_playthrough_id,e.created_at,e.updated_at,e.public_id,e.visibility,e.version,
    (SELECT max(GREATEST(p.started_on,p.finished_on)) FROM playthroughs p WHERE p.user_id=e.user_id AND p.game_id=e.game_id) AS last_played_on
    FROM library_entry_details e JOIN games g ON g.id=e.game_id";

pub(super) const PUBLIC_LIBRARY: &str = "SELECT e.*,
    (SELECT max(GREATEST(p.started_on,p.finished_on)) FROM selected_playthroughs p WHERE p.user_id=e.user_id AND p.game_id=e.game_id) AS last_played_on
    FROM selected_library_entries e";
pub(super) const PUBLIC_CURRENT: &str = "SELECT e.*,
    (SELECT max(GREATEST(p.started_on,p.finished_on)) FROM selected_playthroughs p WHERE p.user_id=e.user_id AND p.game_id=e.game_id) AS last_played_on
    FROM selected_library_entries e WHERE status='playing'";
// The summary view carries its run's selected rating; the selected review is
// joined only so has_review can filter on it.
pub(super) const PUBLIC_PLAYTHROUGHS: &str = "SELECT p.*,v.review FROM selected_playthroughs p
    LEFT JOIN selected_reviews v ON v.user_id=p.user_id AND v.id=p.id";

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_and_query_validation() {
        for date in [None, NaiveDate::from_ymd_opt(2020, 2, 29)] {
            let cursor = Cursor {
                date,
                id: Uuid::new_v4(),
            };
            let parsed = Cursor::parse(&cursor.encode()).unwrap();
            assert_eq!(parsed.date, date);
            assert_eq!(parsed.id, cursor.id);
        }
        for value in [
            "",
            "v2:unknown:00000000-0000-0000-0000-000000000000",
            "v1:2020-02-30:00000000-0000-0000-0000-000000000000",
        ] {
            assert!(Cursor::parse(value).is_err());
        }
        for value in [
            json!({"year":0}),
            json!({"year":10000}),
            json!({"year":2020,"from_on":"2020-01-01"}),
            json!({"undated":true,"year":2020}),
            json!({"from_on":"2021-01-01","to_on":"2020-01-01"}),
        ] {
            let query: BrowseQuery = serde_json::from_value(value).unwrap();
            assert!(query.validate(Kind::Playthroughs).is_err());
        }
        assert_eq!(pattern("  50%_\\  "), "%50\\%\\_\\\\%");
    }
    use serde_json::json;
}
