//! Safety state is private and independent of owner publication choices.
use super::*;
use sqlx::{PgConnection, Postgres, Transaction};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/blocks", get(blocks))
        .route(
            "/api/me/blocks/{id}",
            axum::routing::put(block).delete(unblock),
        )
        .route("/api/me/restrictions", get(own_restrictions))
        .route("/api/me/reports", post(report))
        .route("/api/operator/reports", get(reports))
        .route(
            "/api/operator/reports/{id}",
            get(report_detail).put(resolve_report),
        )
        .route("/api/operator/restrictions", get(restrictions))
        .route(
            "/api/operator/restrictions/{kind}/{id}",
            axum::routing::put(restrict),
        )
        .route("/api/operator/audit", get(audit))
}

#[derive(Clone, Copy, Deserialize, Serialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[ts(export)]
pub enum ModerationTarget {
    Profile,
    Review,
    Note,
    List,
}
impl ModerationTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Review => "review",
            Self::Note => "note",
            Self::List => "list",
        }
    }
}

#[derive(Deserialize, Serialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[ts(export)]
pub enum ReportReason {
    Spam,
    Harassment,
    Hate,
    Sexual,
    Violence,
    Other,
}

#[derive(Deserialize, Serialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[ts(export)]
pub enum ReportStatus {
    Open,
    Resolved,
    Dismissed,
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ReportInput {
    pub kind: ModerationTarget,
    pub target_id: Uuid,
    pub reason: ReportReason,
    #[serde(default)]
    #[ts(optional, as = "Option<String>")]
    pub details: String,
}
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct ReportReceipt {
    pub id: Uuid,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct BlockedProfile {
    pub public_id: Uuid,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct OwnerRestriction {
    pub kind: ModerationTarget,
    pub target_id: Uuid,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct ModerationRestriction {
    pub kind: ModerationTarget,
    pub target_id: Uuid,
    pub active: bool,
    pub version: i32,
}

#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct RestrictionInput {
    pub active: bool,
    pub expected_version: i32,
    pub reason: String,
}
#[derive(Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ReportResolutionInput {
    pub status: ReportStatus,
    pub expected_version: i32,
    pub reason: String,
}

#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct ModerationReport {
    pub id: Uuid,
    pub kind: ModerationTarget,
    pub target_id: Uuid,
    pub reason: ReportReason,
    pub details: String,
    #[ts(type = "unknown | null")]
    pub evidence: Option<serde_json::Value>,
    pub status: ReportStatus,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Serialize, FromRow, TS)]
#[ts(export)]
pub struct ModerationAudit {
    pub id: Uuid,
    pub kind: String,
    pub target_id: Uuid,
    pub action: String,
    pub reason: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Default, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ModerationQuery {
    #[ts(optional)]
    pub limit: Option<u32>,
    #[ts(optional)]
    pub offset: Option<u32>,
}
impl ModerationQuery {
    fn bounds(self) -> ApiResult<(i64, i64)> {
        let limit = self.limit.unwrap_or(20);
        let offset = self.offset.unwrap_or(0);
        if !(1..=50).contains(&limit) || offset > 10_000 {
            return Err(ApiError::invalid(
                "limit must be 1–50 and offset at most 10000",
            ));
        }
        Ok((limit.into(), offset.into()))
    }
}
fn text_bound(value: &str, required: bool) -> ApiResult<()> {
    if value.chars().count() > 2000 || value.contains('\0') || (required && value.trim().is_empty())
    {
        return Err(ApiError::invalid(
            "Text must be at most 2000 characters, without NUL; an operator reason is required",
        ));
    }
    Ok(())
}

// Pair locks are acquired before profile locks in both following and blocking.
// Sorted keys serialize opposite directions without a process-local mutex.
pub(super) async fn lock_pair(conn: &mut PgConnection, a: &[u8], b: &[u8]) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('social:' || encode(LEAST($1::bytea,$2::bytea),'hex') || ':' || encode(GREATEST($1::bytea,$2::bytea),'hex'),0))")
        .bind(a).bind(b).execute(conn).await?;
    Ok(())
}

async fn set_block(
    state: &AppState,
    owner: &[u8],
    id: Uuid,
    enabled: bool,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    let target: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT user_id FROM profiles WHERE public_id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(target) = target {
        if target == owner {
            return Err(ApiError::invalid("You cannot block yourself"));
        }
        lock_pair(&mut tx, owner, &target).await?;
        // Recheck after the lock; deletion must not race the FK insertion.
        let exists: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT user_id FROM profiles WHERE public_id=$1 FOR KEY SHARE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            return if enabled {
                Err(ApiError::not_found())
            } else {
                Ok(StatusCode::NO_CONTENT)
            };
        }
        if enabled {
            sqlx::query("INSERT INTO account_blocks(blocker_id,blocked_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
                .bind(owner).bind(&target).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM follows WHERE (follower_id=$1 AND followed_id=$2) OR (follower_id=$2 AND followed_id=$1)")
                .bind(owner).bind(&target).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM account_blocks WHERE blocker_id=$1 AND blocked_id=$2")
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
async fn block(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<StatusCode> {
    set_block(&state, user.0.id.0.as_bytes(), path(id)?, true).await
}
async fn unblock(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<StatusCode> {
    set_block(&state, user.0.id.0.as_bytes(), path(id)?, false).await
}
async fn blocks(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<ModerationQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<BlockedProfile>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = query.bounds()?;
    Ok(Json(sqlx::query_as("SELECT p.public_id FROM account_blocks b JOIN profiles p ON p.user_id=b.blocked_id WHERE b.blocker_id=$1 ORDER BY p.public_id LIMIT $2 OFFSET $3")
        .bind(user.0.id.0.as_bytes().as_slice()).bind(limit).bind(offset).fetch_all(&state.pool).await?))
}
async fn own_restrictions(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<ModerationQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<OwnerRestriction>>> {
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = query.bounds()?;
    Ok(Json(sqlx::query_as("SELECT kind,target_id FROM moderation_restrictions WHERE owner_id=$1 AND active ORDER BY kind,target_id LIMIT $2 OFFSET $3")
        .bind(user.0.id.0.as_bytes().as_slice()).bind(limit).bind(offset).fetch_all(&state.pool).await?))
}

// Explicit safe evidence, never row_to_json on owner tables. Profile evidence
// includes only its selected identity/bio; list evidence includes public members.
async fn evidence(
    conn: &mut PgConnection,
    kind: ModerationTarget,
    id: Uuid,
) -> ApiResult<(Vec<u8>, serde_json::Value)> {
    let sql = match kind {
        ModerationTarget::Profile => {
            "SELECT user_id,jsonb_build_object('id',public_id,'handle',handle,'display_name',display_name,'bio',CASE WHEN bio_visibility='public' THEN bio END) FROM profiles WHERE public_id=$1 AND public_enabled AND NOT moderation_hidden('profile',public_id)"
        }
        ModerationTarget::Review => {
            "SELECT user_id,to_jsonb(s)-'user_id'-'game_id' FROM public_reviews s WHERE id=$1"
        }
        ModerationTarget::Note => {
            "SELECT user_id,to_jsonb(s)-'user_id'-'game_id' FROM public_notes s WHERE id=$1"
        }
        ModerationTarget::List => {
            "SELECT user_id,(to_jsonb(s)-'user_id') || jsonb_build_object('games',COALESCE((SELECT jsonb_agg(jsonb_build_object('igdb_id',g.igdb_id,'game_name',g.name,'game_slug',g.slug,'cover_url',g.cover_url) ORDER BY m.position) FROM game_list_members m JOIN games g ON g.id=m.game_id WHERE m.list_id=s.id),'[]'::jsonb)) FROM public_lists s WHERE id=$1"
        }
    };
    sqlx::query_as(sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(ApiError::not_found)
}
async fn report(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Body<ReportInput>, ApiError>,
) -> ApiResult<Json<ReportReceipt>> {
    let input = body(payload)?;
    text_bound(&input.details, false)?;
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut tx = state.pool.begin().await?;
    // Serialize deduplication and the per-reporter quota, including new targets.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('report:' || encode($1::bytea,'hex'),0))",
    )
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    // Lock the target account against deletion before capturing public evidence.
    let target = target_owner(&mut tx, input.kind, input.target_id).await?;
    sqlx::query("SELECT id FROM gameboxd_auth.users WHERE id=$1 FOR KEY SHARE")
        .bind(&target)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (_, evidence) = evidence(&mut tx, input.kind, input.target_id).await?;
    if target == owner {
        return Err(ApiError::invalid("You cannot report yourself"));
    }
    let existing = sqlx::query_as(
        "SELECT id FROM moderation_reports WHERE reporter_id=$1 AND kind=$2 AND target_id=$3",
    )
    .bind(owner)
    .bind(input.kind)
    .bind(input.target_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(receipt) = existing {
        return Ok(Json(receipt));
    }
    let count: i64=sqlx::query_scalar("SELECT count(*) FROM moderation_reports WHERE reporter_id=$1 AND created_at>now()-interval '24 hours'")
        .bind(owner).fetch_one(&mut *tx).await?;
    if count >= 20 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::ReportLimit,
            "At most 20 distinct reports per rolling 24 hours",
        ));
    }
    let receipt = ReportReceipt { id: Uuid::new_v4() };
    sqlx::query("INSERT INTO moderation_reports(id,reporter_id,owner_id,kind,target_id,reason,details,evidence) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(receipt.id).bind(owner).bind(target).bind(input.kind).bind(input.target_id).bind(input.reason).bind(input.details).bind(evidence).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}

async fn operator<'a>(
    pool: &'a sqlx::PgPool,
    user: &AuthUser,
) -> ApiResult<Transaction<'a, Postgres>> {
    let mut tx = pool.begin().await?;
    // Every operation authorizes live. Revocation waits for in-flight operations.
    sqlx::query("SELECT user_id FROM moderation_operators WHERE user_id=$1 FOR SHARE")
        .bind(user.0.id.0.as_bytes().as_slice())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "Operator access required",
            )
        })?;
    Ok(tx)
}
async fn target_owner(
    conn: &mut PgConnection,
    kind: ModerationTarget,
    id: Uuid,
) -> ApiResult<Vec<u8>> {
    let sql = match kind {
        ModerationTarget::Profile => "SELECT user_id FROM profiles WHERE public_id=$1",
        ModerationTarget::Review => "SELECT user_id FROM playthroughs WHERE id=$1",
        ModerationTarget::Note => "SELECT user_id FROM diary_notes WHERE id=$1",
        ModerationTarget::List => "SELECT user_id FROM game_lists WHERE id=$1",
    };
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(ApiError::not_found)
}
async fn record_action(
    conn: &mut PgConnection,
    user: &AuthUser,
    owner: Option<&[u8]>,
    kind: &str,
    id: Uuid,
    action: &str,
    reason: &str,
) -> ApiResult<()> {
    sqlx::query("INSERT INTO moderation_audit(id,operator_id,owner_id,kind,target_id,action,reason) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(Uuid::new_v4()).bind(user.0.id.0.as_bytes().as_slice()).bind(owner).bind(kind).bind(id).bind(action).bind(reason).execute(conn).await?;
    Ok(())
}
async fn restrict(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<(ModerationTarget, Uuid)>, PathRejection>,
    payload: Result<Body<RestrictionInput>, ApiError>,
) -> ApiResult<Json<ModerationRestriction>> {
    let (kind, id) = path(id)?;
    let input = body(payload)?;
    text_bound(&input.reason, true)?;
    let mut tx = operator(&state.pool, &user).await?;
    let owner = target_owner(&mut tx, kind, id).await?;
    sqlx::query("SELECT id FROM gameboxd_auth.users WHERE id=$1 FOR KEY SHARE")
        .bind(&owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    // This also serializes with follow writes and prevents account deletion.
    sqlx::query("SELECT user_id FROM profiles WHERE user_id=$1 FOR UPDATE")
        .bind(&owner)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    sqlx::query("INSERT INTO moderation_restrictions(kind,target_id,owner_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(kind).bind(id).bind(&owner).execute(&mut *tx).await?;
    let saved: ModerationRestriction=sqlx::query_as("UPDATE moderation_restrictions SET active=$3,version=version+1 WHERE kind=$1 AND target_id=$2 AND version=$4 RETURNING *")
        .bind(kind).bind(id).bind(input.active).bind(input.expected_version).fetch_optional(&mut *tx).await?.ok_or_else(stale)?;
    record_action(
        &mut tx,
        &user,
        Some(&owner),
        kind.as_str(),
        id,
        if input.active { "restrict" } else { "restore" },
        &input.reason,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(saved))
}
async fn reports(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<ModerationQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<ModerationReport>>> {
    let mut tx = operator(&state.pool, &user).await?;
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = query.bounds()?;
    let rows = sqlx::QueryBuilder::<Postgres>::new(REPORT_SELECT)
        .push(" ORDER BY created_at,id LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset)
        .build_query_as()
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(rows))
}
// Expired evidence is never served, even if the periodic purge is delayed.
const REPORT_SELECT: &str = "SELECT id,kind,target_id,reason,CASE WHEN created_at>now()-interval '90 days' THEN details ELSE '' END AS details,CASE WHEN created_at>now()-interval '90 days' THEN evidence END AS evidence,status,version,created_at FROM moderation_reports";
async fn report_detail(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> ApiResult<Json<ModerationReport>> {
    let mut tx = operator(&state.pool, &user).await?;
    let row = sqlx::QueryBuilder::<Postgres>::new(REPORT_SELECT)
        .push(" WHERE id=")
        .push_bind(path(id)?)
        .build_query_as()
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    tx.commit().await?;
    Ok(Json(row))
}
async fn resolve_report(
    user: AuthUser,
    State(state): State<AppState>,
    id: Result<Path<Uuid>, PathRejection>,
    payload: Result<Body<ReportResolutionInput>, ApiError>,
) -> ApiResult<Json<ModerationReport>> {
    let id = path(id)?;
    let input = body(payload)?;
    text_bound(&input.reason, true)?;
    let mut tx = operator(&state.pool, &user).await?;
    // Lock account before report row, matching deletion's trigger lock order.
    let owner: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT owner_id FROM moderation_reports WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(ApiError::not_found)?;
    if let Some(owner) = &owner {
        sqlx::query("SELECT id FROM gameboxd_auth.users WHERE id=$1 FOR KEY SHARE")
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
    }
    let action = match input.status {
        ReportStatus::Open => "reopen_report",
        ReportStatus::Resolved => "resolve_report",
        ReportStatus::Dismissed => "dismiss_report",
    };
    let saved: Option<(Option<Vec<u8>>,)> = sqlx::query_as("UPDATE moderation_reports SET status=$2,version=version+1 WHERE id=$1 AND version=$3 RETURNING owner_id")
        .bind(id).bind(input.status).bind(input.expected_version).fetch_optional(&mut *tx).await?;
    let (owner,) = saved.ok_or_else(stale)?;
    record_action(
        &mut tx,
        &user,
        owner.as_deref(),
        "report",
        id,
        action,
        if owner.is_some() {
            &input.reason
        } else {
            "Account deleted"
        },
    )
    .await?;
    let row = sqlx::QueryBuilder::<Postgres>::new(REPORT_SELECT)
        .push(" WHERE id=")
        .push_bind(id)
        .build_query_as()
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(row))
}
async fn restrictions(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<ModerationQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<ModerationRestriction>>> {
    let mut tx = operator(&state.pool, &user).await?;
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = query.bounds()?;
    let rows = sqlx::query_as(
        "SELECT * FROM moderation_restrictions ORDER BY kind,target_id LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(rows))
}
async fn audit(
    user: AuthUser,
    State(state): State<AppState>,
    query: Result<Query<ModerationQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<ModerationAudit>>> {
    let mut tx = operator(&state.pool, &user).await?;
    let Query(query) = query.map_err(|_| ApiError::invalid("Invalid query parameters"))?;
    let (limit, offset) = query.bounds()?;
    let rows=sqlx::query_as("SELECT id,kind,target_id,action,CASE WHEN created_at>now()-interval '90 days' THEN reason ELSE 'Expired' END AS reason,created_at FROM moderation_audit ORDER BY created_at,id LIMIT $1 OFFSET $2")
        .bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(rows))
}
