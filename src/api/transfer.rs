//! Versioned personal archives and atomic, create-only merges. Provenance is
//! separate from content: retries neither overwrite later edits nor resurrect
//! deletions. Imported permissions and operational state are never applied.
use super::*;
use crate::models::Visibility;
use chrono::{DateTime, Utc};
use lists::ListKind;
use serde_json::Value;
use sqlx::{PgConnection, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};

const BODY_LIMIT: usize = 32 * 1024 * 1024;
const RECORD_LIMIT: usize = 20_000;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/export", get(export))
        .route("/api/me/import/preview", post(preview))
        .route("/api/me/import", post(apply))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
}

// Archive nulls are explicit: a damaged/missing nullable field must not silently
// turn known content or a date into an unknown value during restoration.
mod required_nullable {
    use super::*;

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::deserialize(deserializer)
    }

    pub fn serialize<S, T>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
        T: Serialize,
    {
        value.serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveBookkeeping {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveProfile {
    pub id: Uuid,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub handle: Option<String>,
    pub display_name: String,
    pub bio: String,
    pub public_enabled: bool,
    pub bio_visibility: Visibility,
    pub statistics_visibility: Visibility,
    #[sqlx(json)]
    pub bookkeeping: ArchiveBookkeeping,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveLibraryEntry {
    pub id: Uuid,
    pub igdb_id: i64,
    pub status: PlayStatus,
    pub visibility: Visibility,
    #[sqlx(json)]
    pub bookkeeping: ArchiveBookkeeping,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchivePlaythrough {
    pub id: Uuid,
    pub igdb_id: i64,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub state: Option<PlaythroughState>,
    pub is_current: bool,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub rating: Option<i16>,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub review: Option<String>,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub presentation_on: Option<NaiveDate>,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub started_on: Option<NaiveDate>,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub finished_on: Option<NaiveDate>,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub platform: Option<String>,
    pub summary_visibility: Visibility,
    pub rating_visibility: Visibility,
    pub review_visibility: Visibility,
    #[sqlx(json)]
    pub bookkeeping: ArchiveBookkeeping,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveNote {
    pub id: Uuid,
    pub playthrough_id: Uuid,
    pub text: String,
    #[serde(with = "required_nullable")]
    #[ts(as = "_")]
    pub presentation_on: Option<NaiveDate>,
    pub visibility: Visibility,
    #[sqlx(json)]
    pub bookkeeping: ArchiveBookkeeping,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveList {
    pub id: Uuid,
    pub kind: ListKind,
    pub title: String,
    pub description: String,
    pub igdb_ids: Vec<i64>,
    pub visibility: Visibility,
    #[sqlx(json)]
    pub bookkeeping: ArchiveBookkeeping,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct ArchiveGame {
    pub igdb_id: i64,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct PersonalArchive {
    pub format: String,
    pub version: u32,
    pub profile: ArchiveProfile,
    pub games: Vec<ArchiveGame>,
    pub library: Vec<ArchiveLibraryEntry>,
    pub playthroughs: Vec<ArchivePlaythrough>,
    pub notes: Vec<ArchiveNote>,
    pub lists: Vec<ArchiveList>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ImportProfilePolicy {
    Restore,
    Keep,
}

#[derive(Deserialize, TS, Serialize)]
#[serde(deny_unknown_fields)]
#[ts(export)]
pub struct PersonalImportInput {
    pub archive: PersonalArchive,
    pub profile_policy: ImportProfilePolicy,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, sqlx::Type, TS,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[ts(export)]
pub enum ImportRecordKind {
    Profile,
    Library,
    Playthrough,
    Note,
    List,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct ImportMapping {
    pub kind: ImportRecordKind,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub already_imported: bool,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct PersonalImportReport {
    pub valid: bool,
    pub applied: bool,
    pub create_count: usize,
    pub skip_count: usize,
    pub missing_game_ids: Vec<i64>,
    pub errors: Vec<FieldError>,
    pub mappings: Vec<ImportMapping>,
}

async fn export(user: AuthUser, State(state): State<AppState>) -> ApiResult<Json<PersonalArchive>> {
    let owner = user.0.id.0.as_bytes().as_slice();
    // Initialize only the ordinary private profile, exactly as GET /api/me does.
    sqlx::query("INSERT INTO profiles(user_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(owner)
        .execute(&state.pool)
        .await?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    // Explicit projections make adding a secret or operational field elsewhere
    // incapable of silently adding it to the portable archive.
    let profile = sqlx::query_as("SELECT public_id AS id,handle,display_name,bio,public_enabled,bio_visibility,statistics_visibility,
        jsonb_build_object('created_at',created_at,'updated_at',updated_at,'version',version) AS bookkeeping
        FROM profiles WHERE user_id=$1").bind(owner).fetch_one(&mut *tx).await?;
    let library = sqlx::query_as("SELECT e.public_id AS id,g.igdb_id,e.status,e.visibility,
        jsonb_build_object('created_at',e.created_at,'updated_at',e.updated_at,'version',e.version) AS bookkeeping
        FROM library_entries e JOIN games g ON g.id=e.game_id WHERE e.user_id=$1 ORDER BY e.public_id")
        .bind(owner).fetch_all(&mut *tx).await?;
    let playthroughs = sqlx::query_as("SELECT p.id,g.igdb_id,p.state,p.is_current,p.rating,p.review,p.presentation_on,p.started_on,p.finished_on,p.platform,
        p.summary_visibility,p.rating_visibility,p.review_visibility,
        jsonb_build_object('created_at',p.created_at,'updated_at',p.updated_at,'version',p.version) AS bookkeeping
        FROM playthroughs p JOIN games g ON g.id=p.game_id WHERE p.user_id=$1 ORDER BY p.id")
        .bind(owner).fetch_all(&mut *tx).await?;
    let notes = sqlx::query_as("SELECT id,playthrough_id,text,presentation_on,visibility,
        jsonb_build_object('created_at',created_at,'updated_at',updated_at,'version',version) AS bookkeeping
        FROM diary_notes WHERE user_id=$1 ORDER BY id").bind(owner).fetch_all(&mut *tx).await?;
    let lists = sqlx::query_as("SELECT l.id,l.kind,l.title,l.description,l.visibility,
        ARRAY(SELECT g.igdb_id FROM game_list_members m JOIN games g ON g.id=m.game_id WHERE m.list_id=l.id ORDER BY m.position) AS igdb_ids,
        jsonb_build_object('created_at',l.created_at,'updated_at',l.updated_at,'version',l.version) AS bookkeeping
        FROM game_lists l WHERE l.user_id=$1 ORDER BY l.id").bind(owner).fetch_all(&mut *tx).await?;
    let games = sqlx::query_as("SELECT igdb_id,name,slug FROM games WHERE id IN (
        SELECT game_id FROM library_entries WHERE user_id=$1 UNION SELECT game_id FROM playthroughs WHERE user_id=$1
        UNION SELECT m.game_id FROM game_list_members m JOIN game_lists l ON l.id=m.list_id WHERE l.user_id=$1) ORDER BY igdb_id")
        .bind(owner).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(PersonalArchive {
        format: "gameboxd.personal".into(),
        version: 1,
        profile,
        games,
        library,
        playthroughs,
        notes,
        lists,
    }))
}

fn issue(errors: &mut Vec<FieldError>, path: impl Into<String>, message: impl Into<String>) {
    errors.push(FieldError::new(
        path,
        FieldErrorCode::InvalidFormat,
        message,
    ));
}
fn text_errors(errors: &mut Vec<FieldError>, path: &str, value: &str, max: usize, required: bool) {
    if value.contains('\0') || value.chars().count() > max || (required && value.trim().is_empty())
    {
        issue(
            errors,
            path,
            format!(
                "Use {}–{max} characters without NUL{}",
                usize::from(required),
                if required { "; must not be blank" } else { "" }
            ),
        );
    }
}
fn unique_ids(errors: &mut Vec<FieldError>, path: &str, ids: impl Iterator<Item = Uuid>) {
    let mut seen = BTreeSet::new();
    for (i, id) in ids.enumerate() {
        if id.is_nil() || !seen.insert(id) {
            issue(
                errors,
                format!("{path}[{i}].id"),
                "Use distinct non-nil UUIDs within each collection",
            );
        }
    }
}

impl PersonalArchive {
    fn game_ids(&self) -> BTreeSet<i64> {
        self.library
            .iter()
            .map(|r| r.igdb_id)
            .chain(self.playthroughs.iter().map(|r| r.igdb_id))
            .chain(self.lists.iter().flat_map(|r| r.igdb_ids.iter().copied()))
            .collect()
    }
    fn validate(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        if self.format != "gameboxd.personal" || self.version != 1 {
            issue(
                &mut errors,
                "archive.version",
                "Expected gameboxd.personal format version 1",
            );
        }
        if self.library.len()
            + self.playthroughs.len()
            + self.notes.len()
            + self.lists.len()
            + self.games.len()
            > RECORD_LIMIT
        {
            issue(
                &mut errors,
                "archive",
                "At most 20000 records, including games, per import",
            );
            return errors;
        }
        if self.profile.id.is_nil() {
            issue(
                &mut errors,
                "archive.profile.id",
                "Source profile ID must not be nil",
            );
        }
        text_errors(
            &mut errors,
            "archive.profile.display_name",
            &self.profile.display_name,
            80,
            false,
        );
        text_errors(
            &mut errors,
            "archive.profile.bio",
            &self.profile.bio,
            1000,
            false,
        );
        unique_ids(
            &mut errors,
            "archive.library",
            self.library.iter().map(|r| r.id),
        );
        unique_ids(
            &mut errors,
            "archive.playthroughs",
            self.playthroughs.iter().map(|r| r.id),
        );
        unique_ids(
            &mut errors,
            "archive.notes",
            self.notes.iter().map(|r| r.id),
        );
        unique_ids(
            &mut errors,
            "archive.lists",
            self.lists.iter().map(|r| r.id),
        );
        let game_ids = self.game_ids();
        let catalog: BTreeSet<_> = self.games.iter().map(|g| g.igdb_id).collect();
        if catalog.len() != self.games.len()
            || catalog != game_ids
            || catalog
                .iter()
                .any(|id| *id <= 0 || *id > 9_007_199_254_740_991)
        {
            issue(
                &mut errors,
                "archive.games",
                "List each referenced positive, JSON-safe IGDB ID exactly once, with no unused games",
            );
        }
        let library: BTreeMap<_, _> = self.library.iter().map(|r| (r.igdb_id, r.status)).collect();
        if library.len() != self.library.len() {
            issue(
                &mut errors,
                "archive.library",
                "Only one library entry per game is allowed",
            );
        }
        let mut current = BTreeSet::new();
        for (i, r) in self.playthroughs.iter().enumerate() {
            let fields = PlaythroughFields {
                state: r.state,
                rating: r.rating,
                review: r.review.clone(),
                presentation_on: r.presentation_on,
                started_on: r.started_on,
                finished_on: r.finished_on,
                platform: r.platform.clone(),
            };
            if let Err(error) = fields.validated() {
                for mut field in error.fields {
                    field.field = format!("archive.playthroughs[{i}].{}", field.field);
                    errors.push(field);
                }
            }
            if r.platform
                .as_ref()
                .is_some_and(|s| s.trim_matches(' ') != s)
            {
                issue(
                    &mut errors,
                    format!("archive.playthroughs[{i}].platform"),
                    "Platform must have no leading or trailing spaces",
                );
            }
            if r.is_current
                && (!matches!(
                    r.state,
                    Some(PlaythroughState::Active | PlaythroughState::Paused)
                ) || r.state.map(|s| s.library_status()) != library.get(&r.igdb_id).copied()
                    || !current.insert(r.igdb_id))
            {
                issue(
                    &mut errors,
                    format!("archive.playthroughs[{i}].is_current"),
                    "One current active/paused run per game, with a matching playing/on_hold library entry, is required",
                );
            }
        }
        let parents: BTreeSet<_> = self.playthroughs.iter().map(|r| r.id).collect();
        for (i, r) in self.notes.iter().enumerate() {
            text_errors(
                &mut errors,
                &format!("archive.notes[{i}].text"),
                &r.text,
                10000,
                true,
            );
            if !parents.contains(&r.playthrough_id) {
                issue(
                    &mut errors,
                    format!("archive.notes[{i}].playthrough_id"),
                    "Parent must be present in archive.playthroughs",
                );
            }
        }
        let mut favorites = false;
        for (i, r) in self.lists.iter().enumerate() {
            text_errors(
                &mut errors,
                &format!("archive.lists[{i}].title"),
                &r.title,
                120,
                true,
            );
            text_errors(
                &mut errors,
                &format!("archive.lists[{i}].description"),
                &r.description,
                5000,
                false,
            );
            if r.igdb_ids.len() > 500
                || r.igdb_ids.iter().collect::<BTreeSet<_>>().len() != r.igdb_ids.len()
            {
                issue(
                    &mut errors,
                    format!("archive.lists[{i}].igdb_ids"),
                    "Use at most 500 distinct games, in the desired order",
                );
            }
            if r.kind == ListKind::Favorites && std::mem::replace(&mut favorites, true) {
                issue(
                    &mut errors,
                    format!("archive.lists[{i}].kind"),
                    "Only one favorites list is allowed",
                );
            }
        }
        errors
    }
}

#[derive(FromRow)]
struct Provenance {
    kind: ImportRecordKind,
    source_id: Uuid,
    target_id: Uuid,
    fingerprint: Vec<u8>,
}
struct Candidate {
    kind: ImportRecordKind,
    source_id: Uuid,
    path: String,
    content: Value,
}
struct Plan {
    report: PersonalImportReport,
    records: Vec<Provenance>,
}

fn candidate<T: Serialize>(kind: ImportRecordKind, id: Uuid, path: String, row: &T) -> Candidate {
    let mut content = serde_json::to_value(row).expect("archive records are serializable");
    let object = content
        .as_object_mut()
        .expect("archive record is an object");
    for key in [
        "id",
        "bookkeeping",
        "visibility",
        "summary_visibility",
        "rating_visibility",
        "review_visibility",
        "public_enabled",
        "bio_visibility",
        "statistics_visibility",
        "handle",
    ] {
        object.remove(key);
    }
    Candidate {
        kind,
        source_id: id,
        path,
        content,
    }
}

async fn plan(
    conn: &mut PgConnection,
    owner: &[u8],
    input: &PersonalImportInput,
) -> ApiResult<Plan> {
    let a = &input.archive;
    let mut report = PersonalImportReport {
        valid: false,
        applied: false,
        create_count: 0,
        skip_count: 0,
        missing_game_ids: vec![],
        errors: a.validate(),
        mappings: vec![],
    };
    if !report.errors.is_empty() {
        return Ok(Plan {
            report,
            records: vec![],
        });
    }
    let existing: Vec<Provenance> = sqlx::query_as("SELECT kind,source_id,target_id,fingerprint FROM personal_import_records WHERE user_id=$1 AND source_profile_id=$2")
        .bind(owner).bind(a.profile.id).fetch_all(&mut *conn).await?;
    let existing: BTreeMap<_, _> = existing
        .into_iter()
        .map(|r| ((r.kind, r.source_id), r))
        .collect();
    let profile: Option<Profile> = sqlx::query_as("SELECT * FROM profiles WHERE user_id=$1")
        .bind(owner)
        .fetch_optional(&mut *conn)
        .await?;
    if profile
        .as_ref()
        .is_some_and(|p| p.public_id == a.profile.id)
    {
        issue(
            &mut report.errors,
            "archive.profile.id",
            "This is the source account; import into another account instead of duplicating its own history",
        );
    }
    let mut candidates = Vec::new();
    if input.profile_policy == ImportProfilePolicy::Restore {
        candidates.push(candidate(
            ImportRecordKind::Profile,
            a.profile.id,
            "archive.profile".into(),
            &a.profile,
        ));
    }
    for (i, r) in a.library.iter().enumerate() {
        candidates.push(candidate(
            ImportRecordKind::Library,
            r.id,
            format!("archive.library[{i}]"),
            r,
        ));
    }
    for (i, r) in a.playthroughs.iter().enumerate() {
        candidates.push(candidate(
            ImportRecordKind::Playthrough,
            r.id,
            format!("archive.playthroughs[{i}]"),
            r,
        ));
    }
    for (i, r) in a.notes.iter().enumerate() {
        candidates.push(candidate(
            ImportRecordKind::Note,
            r.id,
            format!("archive.notes[{i}]"),
            r,
        ));
    }
    for (i, r) in a.lists.iter().enumerate() {
        candidates.push(candidate(
            ImportRecordKind::List,
            r.id,
            format!("archive.lists[{i}]"),
            r,
        ));
    }
    let library: Vec<(i64, PlayStatus)> = sqlx::query_as("SELECT g.igdb_id,e.status FROM library_entries e JOIN games g ON g.id=e.game_id WHERE e.user_id=$1")
        .bind(owner).fetch_all(&mut *conn).await?;
    let library: BTreeMap<_, _> = library.into_iter().collect();
    let runs: Vec<(Uuid, i64, bool)> = sqlx::query_as("SELECT p.id,g.igdb_id,p.is_current FROM playthroughs p JOIN games g ON g.id=p.game_id WHERE p.user_id=$1")
        .bind(owner).fetch_all(&mut *conn).await?;
    let parent_ids: BTreeSet<_> = runs.iter().map(|r| r.0).collect();
    let current_games: BTreeSet<_> = runs.iter().filter(|r| r.2).map(|r| r.1).collect();
    let favorites: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM game_lists WHERE user_id=$1 AND kind='favorites')",
    )
    .bind(owner)
    .fetch_one(&mut *conn)
    .await?;
    // Sorted bijection preserves UUID tie order for a fresh round trip. New
    // IDs are provisional until commit; preview mappings include only skips.
    candidates.sort_by_key(|r| (r.source_id, r.kind));
    let mut new_ids: Vec<_> = (0..candidates.len()).map(|_| Uuid::new_v4()).collect();
    new_ids.sort();
    let mut records = Vec::new();
    let contents: Vec<_> = candidates.iter().map(|c| &c.content).collect();
    let fingerprints: Vec<Vec<u8>> = sqlx::query_scalar("SELECT sha256(convert_to(value::text,'UTF8')) FROM jsonb_array_elements($1) WITH ORDINALITY ORDER BY ordinality")
        .bind(sqlx::types::Json(contents)).fetch_all(&mut *conn).await?;
    for ((c, new_id), fingerprint) in candidates.into_iter().zip(new_ids).zip(fingerprints) {
        if let Some(old) = existing.get(&(c.kind, c.source_id)) {
            if old.fingerprint != fingerprint {
                issue(
                    &mut report.errors,
                    &c.path,
                    "Previously imported source record has changed; create-only merge never overwrites it. Resolve changes through ordinary revision-protected edits",
                );
            }
            report.skip_count += 1;
            report.mappings.push(ImportMapping {
                kind: c.kind,
                source_id: c.source_id,
                target_id: old.target_id,
                already_imported: true,
            });
            continue;
        }
        let target_id = if c.kind == ImportRecordKind::Profile {
            if profile.as_ref().is_some_and(|p| {
                !p.display_name.is_empty()
                    || !p.bio.is_empty()
                    || p.handle.is_some()
                    || p.public_enabled
                    || p.bio_visibility != Visibility::Private
                    || p.statistics_visibility != Visibility::Private
            }) {
                issue(
                    &mut report.errors,
                    &c.path,
                    "Profile restore requires a blank private destination profile; choose profile_policy=keep to preserve it",
                );
            }
            profile.as_ref().map_or(new_id, |p| p.public_id)
        } else {
            new_id
        };
        if c.kind == ImportRecordKind::Library {
            let occupied =
                library.contains_key(&c.content["igdb_id"].as_i64().expect("typed game ID"));
            if occupied {
                issue(
                    &mut report.errors,
                    &c.path,
                    "Destination already tracks this game; existing library status and visibility will not be overwritten",
                );
            }
        }
        if c.kind == ImportRecordKind::Playthrough && c.content["is_current"] == true {
            let game = c.content["igdb_id"].as_i64().expect("typed game ID");
            let occupied = current_games.contains(&game);
            // A previously imported membership may have been deleted or changed.
            // Skipping its provenance must not install a mismatched current run.
            let entry = a
                .library
                .iter()
                .find(|r| r.igdb_id == game)
                .expect("validated current membership");
            if existing.contains_key(&(ImportRecordKind::Library, entry.id))
                && library.get(&game) != Some(&entry.status)
            {
                issue(
                    &mut report.errors,
                    &c.path,
                    "Previously imported library status changed or was removed; a current run cannot be restored against it",
                );
            }
            if occupied {
                issue(
                    &mut report.errors,
                    &c.path,
                    "Destination already has a current run for this game",
                );
            }
        }
        if c.kind == ImportRecordKind::List && c.content["kind"] == "favorites" && favorites {
            issue(
                &mut report.errors,
                &c.path,
                "Destination already has favorites; merge membership manually using the list revision",
            );
        }
        report.create_count += 1;
        records.push(Provenance {
            kind: c.kind,
            source_id: c.source_id,
            target_id,
            fingerprint,
        });
    }
    // New children may reference earlier imports, but never resurrect deleted
    // parents or attach to another owner through a file-supplied ID.
    let new_records: BTreeSet<_> = records.iter().map(|r| (r.kind, r.source_id)).collect();
    for (i, n) in a.notes.iter().enumerate() {
        if new_records.contains(&(ImportRecordKind::Note, n.id))
            && let Some(parent) = existing.get(&(ImportRecordKind::Playthrough, n.playthrough_id))
            && !parent_ids.contains(&parent.target_id)
        {
            issue(
                &mut report.errors,
                format!("archive.notes[{i}].playthrough_id"),
                "Previously imported parent was deleted; it will not be resurrected",
            );
        }
    }
    let referenced: Vec<_> = a.game_ids().into_iter().collect();
    report.missing_game_ids = sqlx::query_scalar("SELECT igdb_id FROM unnest($1::bigint[]) u(igdb_id) WHERE NOT EXISTS(SELECT 1 FROM games g WHERE g.igdb_id=u.igdb_id) ORDER BY igdb_id")
        .bind(referenced).fetch_all(&mut *conn).await?;
    report.valid = report.errors.is_empty();
    Ok(Plan { report, records })
}

async fn preview(
    user: AuthUser,
    State(state): State<AppState>,
    payload: Result<Body<PersonalImportInput>, ApiError>,
) -> ApiResult<Json<PersonalImportReport>> {
    let input = body(payload)?;
    let mut tx = browsing::snapshot(&state.pool).await?;
    let plan = plan(&mut tx, user.0.id.0.as_bytes(), &input).await?;
    tx.commit().await?;
    Ok(Json(plan.report))
}

fn rejected(report: PersonalImportReport) -> ApiError {
    let mut error = ApiError::new(
        StatusCode::CONFLICT,
        ErrorCode::ImportConflict,
        "Nothing was imported. Resolve the reported conflicts and run preview again",
    );
    error.fields = report.errors;
    error
}

async fn apply(
    user: AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Body<PersonalImportInput>, ApiError>,
) -> ApiResult<Response> {
    let input = body(payload)?;
    let receipt = retry::Receipt::new(&headers, "personal:import", &input)?;
    let errors = input.archive.validate();
    if !errors.is_empty() {
        return Err(ApiError::fields(errors));
    }

    if let Some(response) = receipt.cached(&state.pool, user.0.id.0.as_bytes()).await? {
        return Ok(response);
    }
    let owner = user.0.id.0.as_bytes().as_slice();
    let mut check = browsing::snapshot(&state.pool).await?;
    let initial = plan(&mut check, owner, &input).await?;
    check.commit().await?;
    if !initial.report.valid {
        return Err(rejected(initial.report));
    }
    // Fetch trusted metadata before acquiring write locks. A supplied archive
    // cannot poison the shared catalog, and failure creates no partial records.
    let mut games = Vec::new();
    for id in &initial.report.missing_game_ids {
        games.push(state.igdb.get(*id).await.map_err(|mut error| {
            error.fields.push(FieldError::new("archive.games", FieldErrorCode::InvalidFormat,
                format!("Could not resolve IGDB game {id}; nothing was imported. Resolve the provider error and retry")));
            error
        })?);
    }
    let mut tx = state.pool.begin().await?;
    if let Some(response) = receipt.replay(&mut tx, user.0.id.0.as_bytes()).await? {
        return Ok(response);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('gameboxd:import:' || encode($1::bytea,'hex'),0))")
        .bind(owner).execute(&mut *tx).await?;
    for id in input.archive.game_ids() {
        lock_igdb_game(&mut tx, owner, id).await?;
    }
    sqlx::query("INSERT INTO profiles(user_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT user_id FROM profiles WHERE user_id=$1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await?;
    let mut plan = plan(&mut tx, owner, &input).await?;
    if !plan.report.valid {
        return Err(rejected(plan.report));
    }
    for game in games {
        sqlx::query("INSERT INTO games(id,igdb_id,name,slug,summary,cover_url,first_release_date,platforms,genres,developers,game_type,parent_igdb_id,version_parent_igdb_id,source_url)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT(igdb_id) DO NOTHING")
            .bind(Uuid::new_v4()).bind(game.id).bind(&game.name).bind(&game.slug).bind(&game.summary).bind(game.cover_url()).bind(game.release_date())
            .bind(sqlx::types::Json(&game.platforms)).bind(sqlx::types::Json(&game.genres)).bind(sqlx::types::Json(game.developers()))
            .bind(game.game_type).bind(game.parent_game).bind(game.version_parent).bind(game.source_url()).execute(&mut *tx).await?;
    }
    if let Err(error) = write(&mut tx, owner, &input, &mut plan).await {
        // Uniqueness/FK races with normal creation/deletion reject the entire
        // merge, including catalog rows and provenance. Rerunning preview tells
        // the caller the new conflict; retry cannot leave half an import.
        if matches!(&error, sqlx::Error::Database(e) if e.is_unique_violation() || e.is_foreign_key_violation())
        {
            issue(
                &mut plan.report.errors,
                "archive",
                "Destination changed during import; nothing was imported. Run preview again",
            );
            plan.report.valid = false;
            plan.report.mappings.retain(|m| m.already_imported);
            return Err(rejected(plan.report));
        }
        return Err(error.into());
    }
    plan.report.applied = true;
    plan.report.missing_game_ids.clear();
    receipt
        .finish(tx, user.0.id.0.as_bytes(), Json(plan.report))
        .await
}

async fn write(
    tx: &mut Transaction<'_, Postgres>,
    owner: &[u8],
    input: &PersonalImportInput,
    plan: &mut Plan,
) -> Result<(), sqlx::Error> {
    let a = &input.archive;
    let ids: BTreeMap<_, _> = plan
        .records
        .iter()
        .map(|r| ((r.kind, r.source_id), r.target_id))
        .chain(
            plan.report
                .mappings
                .iter()
                .map(|r| ((r.kind, r.source_id), r.target_id)),
        )
        .collect();
    let creates: BTreeSet<_> = plan.records.iter().map(|r| (r.kind, r.source_id)).collect();
    if creates.contains(&(ImportRecordKind::Profile, a.profile.id)) {
        sqlx::query("UPDATE profiles SET display_name=$2,bio=$3 WHERE user_id=$1")
            .bind(owner)
            .bind(&a.profile.display_name)
            .bind(&a.profile.bio)
            .execute(&mut **tx)
            .await?;
    }
    for r in &a.library {
        if !creates.contains(&(ImportRecordKind::Library, r.id)) {
            continue;
        }
        sqlx::query("INSERT INTO library_entries(user_id,game_id,public_id,status) VALUES($1,(SELECT id FROM games WHERE igdb_id=$2),$3,$4)")
            .bind(owner).bind(r.igdb_id).bind(ids[&(ImportRecordKind::Library,r.id)]).bind(r.status).execute(&mut **tx).await?;
    }
    for r in &a.playthroughs {
        if !creates.contains(&(ImportRecordKind::Playthrough, r.id)) {
            continue;
        }
        sqlx::query("INSERT INTO playthroughs(id,user_id,game_id,state,is_current,rating,review,presentation_on,started_on,finished_on,platform)
            VALUES($1,$2,(SELECT id FROM games WHERE igdb_id=$3),$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(ids[&(ImportRecordKind::Playthrough,r.id)]).bind(owner).bind(r.igdb_id).bind(r.state).bind(r.is_current).bind(r.rating).bind(&r.review)
            .bind(r.presentation_on).bind(r.started_on).bind(r.finished_on).bind(&r.platform).execute(&mut **tx).await?;
    }
    for r in &a.notes {
        if !creates.contains(&(ImportRecordKind::Note, r.id)) {
            continue;
        }
        sqlx::query("INSERT INTO diary_notes(id,user_id,playthrough_id,text,presentation_on) VALUES($1,$2,$3,$4,$5)")
            .bind(ids[&(ImportRecordKind::Note,r.id)]).bind(owner).bind(ids[&(ImportRecordKind::Playthrough,r.playthrough_id)])
            .bind(&r.text).bind(r.presentation_on).execute(&mut **tx).await?;
    }
    for r in &a.lists {
        if !creates.contains(&(ImportRecordKind::List, r.id)) {
            continue;
        }
        let id = ids[&(ImportRecordKind::List, r.id)];
        sqlx::query(
            "INSERT INTO game_lists(id,user_id,kind,title,description) VALUES($1,$2,$3,$4,$5)",
        )
        .bind(id)
        .bind(owner)
        .bind(r.kind)
        .bind(&r.title)
        .bind(&r.description)
        .execute(&mut **tx)
        .await?;
        sqlx::query("INSERT INTO game_list_members(list_id,game_id,position) SELECT $1,g.id,(u.position-1)::int FROM unnest($2::bigint[]) WITH ORDINALITY u(igdb_id,position) JOIN games g USING(igdb_id)")
            .bind(id).bind(&r.igdb_ids).execute(&mut **tx).await?;
    }
    for r in &plan.records {
        sqlx::query("INSERT INTO personal_import_records(user_id,source_profile_id,kind,source_id,target_id,fingerprint) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(owner).bind(a.profile.id).bind(r.kind).bind(r.source_id).bind(r.target_id).bind(&r.fingerprint).execute(&mut **tx).await?;
        plan.report.mappings.push(ImportMapping {
            kind: r.kind,
            source_id: r.source_id,
            target_id: r.target_id,
            already_imported: false,
        });
    }
    Ok(())
}
