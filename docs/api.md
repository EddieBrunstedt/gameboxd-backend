# API contract

Base URL: `http://localhost:3000`. All bodies are JSON unless a response is `204 No Content`. Timestamps use RFC 3339 UTC. Games are addressed by their IGDB integer ID everywhere, including in library routes; the local UUID in a game object is an internal primary key and never appears in a path. Playthroughs have their own UUIDs, used in playthrough paths. Request bodies are limited to 64 KiB.

## Application routes

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| GET | `/health/live` | No | Process liveness |
| GET | `/health/ready` | No | PostgreSQL readiness |
| GET | `/api/me` | Yes | Current user ID, email and app profile |
| PUT | `/api/me` | Yes | Replace display name and bio |
| GET | `/api/games?q=halo&limit=20&offset=0` | No | Search games stored locally |
| GET | `/api/games/{igdb_id}` | No | Locally stored game details |
| GET | `/api/catalog/search?q=halo&limit=20&offset=0` | No | Search IGDB; read-only, stores nothing |
| GET | `/api/catalog/games/{igdb_id}` | No | Look up IGDB game details; read-only, stores nothing |
| POST | `/api/catalog/import/{igdb_id}` | Yes | Store one game, or refresh metadata older than 24 hours in line |
| GET | `/api/me/library?status=playing&q=halo&limit=20&offset=0` | Yes | Own library with nested game details |
| GET | `/api/me/library/{igdb_id}` | Yes | Own entry for a game |
| PUT | `/api/me/library/{igdb_id}` | Yes | Create or replace own entry, storing the game if needed |
| DELETE | `/api/me/library/{igdb_id}` | Yes | Remove own library entry, preserving playthroughs (idempotent 204) |
| GET | `/api/me/playthroughs?igdb_id=42&limit=20&offset=0` | Yes | Own diary; optional game filter |
| POST | `/api/me/playthroughs` | Yes | Record a new experience, storing the game if needed (201) |
| POST | `/api/me/playthroughs/start` | Yes | Start a current run or replay and set library status (201) |
| POST | `/api/me/playthroughs/{id}/lifecycle` | Yes | Pause, resume, complete or abandon; requires `If-Match` |
| GET | `/api/me/playthroughs/{id}` | Yes | Own playthrough |
| PUT | `/api/me/playthroughs/{id}` | Yes | Replace every editable field; requires `If-Match` |
| PATCH | `/api/me/playthroughs/{id}` | Yes | Change only the fields present; requires `If-Match` |
| DELETE | `/api/me/playthroughs/{id}` | Yes | Delete own playthrough (idempotent 204; `If-Match` optional) |
| GET | `/api/me/diary?limit=20&offset=0` | Yes | Own diary notes across all playthroughs, with game context |
| GET | `/api/me/playthroughs/{id}/notes?limit=20&offset=0` | Yes | Own notes for one playthrough |
| POST | `/api/me/playthroughs/{id}/notes` | Yes | Create a private note (201) |
| GET | `/api/me/notes/{id}` | Yes | Own note, with revision ETag |
| PUT | `/api/me/notes/{id}` | Yes | Replace note text/date; requires `If-Match` |
| PATCH | `/api/me/notes/{id}` | Yes | Edit note text/date; requires `If-Match` |
| DELETE | `/api/me/notes/{id}` | Yes | Delete note (204); requires `If-Match` |
| PUT | `/api/me/publication` | Yes | Enable/disable public profile; requires profile `If-Match` |
| PUT | `/api/me/library/{igdb_id}/visibility` | Yes | Set library entry visibility; requires entry `If-Match` |
| PUT | `/api/me/playthroughs/{id}/visibility` | Yes | Set independent summary/rating/review visibility; requires playthrough `If-Match` |
| PUT | `/api/me/notes/{id}/visibility` | Yes | Set note visibility; requires note `If-Match` |
| GET | `/api/public/library/{id}` | No | Public library entry, addressed by its `public_id` |
| GET | `/api/public/playthroughs/{id}` | No | Public playthrough summary |
| GET | `/api/public/ratings/{id}` | No | Public rating, addressed by playthrough UUID |
| GET | `/api/public/reviews/{id}` | No | Public review, addressed by playthrough UUID |
| GET | `/api/public/notes/{id}` | No | Public diary note |
| PUT | `/api/me/handle` | Yes | Claim/rename a handle; requires profile `If-Match` |
| GET | `/api/public/profiles/{id}` | No | Public profile header and selected counts |
| GET | `/api/public/handles/{handle}` | No | Resolve current handle to public profile |
| GET | `/api/public/profiles/{id}/{section}` | No | Paginated public profile section |
| GET | `/api/me/public-preview` | Yes | Session owner's visitor profile, including while disabled |
| GET | `/api/me/public-preview/{section}` | Yes | Session owner's selected visitor section |
| GET | `/api/me/lists` | Yes | Paginated own lists, including favorites |
| POST | `/api/me/lists` | Yes | Create an empty private list (201) |
| GET | `/api/me/lists/{id}` | Yes | Own list and all ordered members, with revision ETag |
| PUT | `/api/me/lists/{id}` | Yes | Replace title/description; requires revision `If-Match` |
| PUT | `/api/me/lists/{id}/games` | Yes | Replace ordered membership; requires revision `If-Match` |
| PUT | `/api/me/lists/{id}/visibility` | Yes | Set list visibility; requires revision `If-Match` |
| DELETE | `/api/me/lists/{id}` | Yes | Delete list and membership (204); requires revision `If-Match` |
| GET | `/api/public/lists/{id}` | No | Public list and ordered game context |
| GET | `/api/me/public-preview/lists/{id}` | Yes | Visitor detail of owner's selected list, including while profile is disabled |

The owner-only stored game shape (authenticated import response and nested owner library game) includes `id`, `igdb_id`, `name`, `slug`, nullable `summary`, `cover_url`, `first_release_date`, arrays of `{id,name}` for `platforms`, `genres`, `developers`, nullable `game_type`, `parent_igdb_id`, `version_parent_igdb_id`, `source_url`, `imported_at` and `refreshed_at`. Taxonomy/parent IDs are upstream IGDB IDs, not local UUIDs. The `game` nested in an owner library listing is that same object, field for field. Public local game reads (`GET /api/games` and `/api/games/{igdb_id}`) instead return the descriptive catalog shape below, without local identity or import/refresh bookkeeping.

Catalog search results and catalog lookup share one descriptive shape: `igdb_id`, `name`, `slug`, nullable `summary`, `cover_url`, `first_release_date`, arrays of `{id,name}` for `platforms`, `genres`, `developers`, nullable `game_type`, `parent_igdb_id`, `version_parent_igdb_id`, and `source_url`. They carry no `id`, `imported_at` or `refreshed_at`, because a viewed game need not exist locally. Address them by `igdb_id`.

Use `GET /api/catalog/games/{igdb_id}` for frontend detail pages, including direct links and reloads. It returns a single catalog game object and needs no prior search, import, or sign-in. Search and lookup always query IGDB and never insert or refresh local games, create library entries, or record playthroughs. Imports and personal records still require authentication.

Catalog lookup accepts a positive signed 64-bit integer ID; malformed, overflowing, zero and negative IDs return `400 validation_error` before contacting IGDB. A game absent from IGDB returns `404 not_found`; provider failures retain the IGDB errors below, including `503 igdb_not_configured` when credentials are missing. Catalog lookup has no local fallback. Use `GET /api/games/{igdb_id}` when specifically reading stored metadata without IGDB; that route still returns 404 for games not stored here.

`PUT /api/me` body (both fields required):

```json
{"display_name":"Eddie","bio":"I like RPGs."}
```

Display name is trimmed and limited to 80 characters; bio to 1000. The response is the profile object with those fields plus `created_at`, `updated_at`, `version`, `public_enabled` (initially false), random `public_id`, nullable `handle`, and `bio_visibility` (initially `private`). `GET /api/me` returns `{id,email,profile}`, where `id` is the Doorman user UUID and `email` may be null.

`PUT /api/me/library/{igdb_id}` body:

```json
{"status":"completed"}
```

Allowed statuses: `backlog`, `playing`, `completed`, `on_hold`, `dropped`. `status` is required and is the only field accepted by the status endpoint. Visibility is edited through its separate endpoint. Each entry returns `igdb_id`, `status`, derived nullable `rating`, nullable `rating_playthrough_id`, nullable `current_playthrough_id`, `created_at`, `updated_at`, `public_id`, `visibility`, and `version`. Ratings and reviews cannot be written through this route; obsolete payloads containing either field are rejected rather than silently discarded. The library has no review field.

The displayed rating comes from the first **rated** playthrough, using the chronology defined below. `rating_playthrough_id` identifies that experience. An unrated replay does not hide a previous rating. Editing or deleting a playthrough recalculates the displayed rating automatically; if none remain rated, both derived fields are null. Status-only saves never edit a playthrough. While a current run exists, a status-only save must agree with its state (`active` → `playing`, `paused` → `on_hold`), otherwise it returns `409 lifecycle_conflict`; use a lifecycle action to change both together. Library timestamps and revisions describe membership/status/visibility writes, not changes to the derived rating.

Saving an entry stores an unknown game automatically, so no import call is needed first. A game already stored is reused exactly as it stands, however old its metadata, so this save never waits for IGDB — not for its request spacing, its rate limit, or a timeout while it is down. Only a game never stored here requires a successful IGDB fetch; an IGDB ID no game exists under returns 404, and a provider failure returns the IGDB errors below. The same applies to recording a playthrough.

Stale metadata found this way is queued for a background refresh instead, so the game is brought up to date shortly after the save rather than during it. The refresh is invisible to the client: it changes no response, and its failure is not reported, because the save it followed has already succeeded. Refreshing happens in place, so records saved a moment earlier stay attached to the same game. `POST /api/catalog/import/{igdb_id}` remains the request that waits for a refresh, which is the point of asking for one; it falls back to the stored game when the provider fails.

## Playthroughs and diary

A playthrough represents one experience with a game, not an individual gaming session. Multiple entries for the same game are allowed. Completion is never required: abandoned runs and open-ended games can be rated or reviewed. Historical recording and library membership are independent: `POST /api/me/playthroughs` does not create a library entry or guess its status; removing a library entry preserves history. Deleting an account removes both.

`POST /api/me/playthroughs` body (`igdb_id` required, all other fields optional):

```json
{
  "igdb_id": 42,
  "rating": 9,
  "review": "A memorable first playthrough.",
  "presentation_on": "2022-04-15",
  "started_on": "2022-03-01",
  "finished_on": "2022-04-15",
  "platform": "PC"
}
```

`PUT` and `PATCH /api/me/playthroughs/{id}` accept the same editable fields **without** `igdb_id`. The owner, game, UUID and recording time cannot be changed. The two differ only in how an omitted field is read:

- **PUT replaces.** Every editable field takes the value in the body, so an omitted field and an explicit `null` both clear it. Use it where one form owns the whole record.
- **PATCH merges.** An omitted field keeps its stored value and an explicit `null` clears it. Use it where a control owns a single field — a rating widget, a review editor, a date picker — so it cannot erase fields it never displayed.

Both preserve the playthrough's identity and `created_at`. Unknown fields are rejected by either route, so a misspelled name fails loudly instead of silently changing nothing. An edit that changes nothing is not a revision: `updated_at`, `version` and the entity tag all stay as they were. To start a current replay, use the start route below. To backfill a past replay, POST a new historical entry. POST always creates a new UUID, so retrying a successful POST can create a duplicate.

- `rating`: null or an integer 1–10 (divide by two to show half-stars).
- `review`: null or plain text of at most 10000 characters, without NUL. Render user content as text, never as HTML.
- `presentation_on`: null or an owner-chosen historical calendar date (`YYYY-MM-DD`) for this entry, including its rating/review. Optional on creation; omitted means unknown. Independent of start/end dates, with no requirement to fall between them. Backdates and future dates are allowed. See the chronology policy below.
- `state`: `active`, `paused`, `completed`, `abandoned`, or null (unknown). Creation/PUT omission means unknown; PATCH omission preserves it and null clears it. State describes the experience independently of its chosen historical date. Current runs must change state through lifecycle actions; historical state is freely correctable through PUT/PATCH.
- `is_current`: read-only boolean. Only start and lifecycle/removal operations control it. It is not inferred from state or dates.
- `started_on`, `finished_on`: null or calendar dates in `YYYY-MM-DD` format. If both are set, the finish must be on or after the start. These describe the experience; `finished_on` does not imply the game was completed or update library status. Either date can be provided alone. Active/paused runs cannot have an end date; completed/abandoned runs may have an unknown end date. For a historical correction to active/paused, clear an existing end date in the same edit.
- `platform`: null or a label of 1–100 characters without control characters, trimmed when saved. This is free text (for example `PC` or `Steam Deck`), not an IGDB taxonomy ID.

An undated rating, a review without a rating, and a record with neither are all valid. Responses contain `id`, `igdb_id`, `version`, `rating`, `review`, `presentation_on`, `started_on`, `finished_on`, `platform`, `created_at`, and `updated_at`; they never expose the owner or local game key.

### Concurrent and partial edits

Every playthrough carries an integer `version`, and single-resource responses — read, create, replace and update alike — repeat it as a strong `ETag` such as `"3"`. Listings carry `version` on each item, so an editor opened from a list needs no extra read first.

`PUT`, `PATCH`, and lifecycle actions **require** an `If-Match` header naming the revision the edit was based on. Without one the request is refused with `428 precondition_required` instead of being allowed to overwrite work the client never saw. A precondition that no longer holds returns `412 precondition_failed`: reload the playthrough, reapply the change, and retry. `If-Match: *` asserts only that the playthrough still exists and accepts whatever revision that is. Weak tags such as `W/"3"` parse but can never satisfy the precondition, because `If-Match` compares entity tags strongly. A header that cannot be parsed is a `400 validation_error`, never a silently absent precondition.

`DELETE` takes `If-Match` optionally. Without one it stays idempotent and answers 204 whether or not the playthrough was there. With one it asserts a revision instead, so a playthrough that is already gone or has since changed returns 412.

Ownership is resolved before the precondition, so another user's playthrough answers 404 for reads, edits, and lifecycle actions and never discloses a revision through 412. Concurrent edits to one playthrough are serialized: a partial edit merges into the revision it was checked against, never into a value a competing write has already replaced. Validation that spans fields — `finished_on` against `started_on` — is applied to the merged record, so a patch cannot move one date past a stored one.

Library entries carry no entity tag. `status` is the only writable field there, so each `PUT` sends a complete intent and can discard nothing; the derived `rating` in its response is owned by playthroughs, so a tag on that route would not describe the representation it returned. Ratings and reviews are edited through the playthrough routes, where the preconditions above apply.

### Dated diary notes

Each playthrough can have multiple independent plain-text notes alongside its overall rating/review. Notes can be recorded on any run, including completed, abandoned, detached, or undated runs. The `/api/me` note routes are authenticated and owner-only. Notes start private and have separate visibility controls and public projections described below.

`POST /api/me/playthroughs/{id}/notes` and `PUT /api/me/notes/{id}` accept:

```json
{"text":"Reached the mountain village.\nThe soundtrack is excellent.","presentation_on":"2020-06-15"}
```

`text` is required, non-null, nonblank, and limited to 10,000 Unicode characters. NUL is rejected. Text is stored verbatim, preserving line breaks and surrounding whitespace; render it as plain text. `presentation_on` is an optional nullable calendar day. Omission on POST/PUT means unknown. `PATCH` preserves omitted fields; an explicit null clears only the date, never text. An empty PATCH is a no-op. Unknown fields, including owner, parent, revision, and visibility, are rejected. Neither parent nor game can be reassigned.

A `DiaryNote` returns `id`, `playthrough_id`, `igdb_id`, `game_name`, `game_slug`, nullable `cover_url`, `text`, nullable `presentation_on`, `version`, `created_at`, and `updated_at`. All game context comes from the local catalog without IGDB requests or refresh dependencies. Single-note GET and successful POST/PUT/PATCH include an ETag containing the quoted decimal note version. Lists include the same body shape and version.

PUT, PATCH, and DELETE require `If-Match`: missing is `428 precondition_required`, malformed is `400 validation_error`, and a stale or weak-only tag is `412 precondition_failed`. Lists of strong tags and `*` follow the playthrough precondition rules; `*` explicitly accepts whichever version exists. Missing/other-owner notes and playthroughs return `404 not_found`, including nested note lists, without revealing revisions. A deleted note returns 404 on a subsequent conditional deletion. Schema failures return `422 invalid_json`; invalid text returns field-specific `400 validation_error`.

`GET /api/me/diary` lists notes across all of the owner's playthroughs. `GET /api/me/playthroughs/{id}/notes` lists that owner's notes for one run; an existing run without notes returns an empty page. Both return `BrowsePage<DiaryNote>` with filtered totals and date cursors; see [browsing](#library-and-diary-browsing) for filters and pagination. The diary is a note collection; overall experiences/reviews remain in `/api/me/playthroughs`.

Both lists use `presentation_on DESC NULLS LAST, id DESC`, exactly the [historical chronology policy](#historical-chronology-policy). Note dates are independent of parent presentation/start/end dates, with no inferred dates, range restrictions, or timezone conversion. Backdates and future dates are allowed. Text edits preserve historical position; date edits reposition the note. Unknown dates remain at the end with deterministic random-UUID ties. Actual write/edit timestamps never order notes. Offset pages reflect current data on each read, so intervening inserts, deletes, or date corrections can shift boundaries; restart paging after such changes. Text-only edits do not shift boundaries. Cursor continuation and filters are described in [browsing](#library-and-diary-browsing).

Notes have independent revisions: changing a note does not change the parent's revision, review, rating, lifecycle, or library status. No-op writes preserve the note's timestamps and revision. Note writes lock the parent against deletion before locking the note, so simultaneous edits merge/check against the locked revision and parent deletion cannot leave orphaned rows. They do not participate in current-run state changes.

Removing library membership or starting a replay preserves all previous notes on their original playthrough IDs. Deleting a playthrough deliberately cascades to **all its notes** in the same transaction; deleting the account removes them too. The playthrough's own revision does not cover its notes: deleting that aggregate removes even independently revised notes. Note-only deletion checks the note's own revision and preserves the parent/review/siblings. Creation always uses a new UUID; uncertain POST retries can duplicate notes until feature 12 adds idempotency keys.

Migration `202609090003_diary_notes.sql` adds an empty note table without deriving notes from reviews, inventing historical dates, changing existing playthrough content/revisions, or publishing data. Independent note visibility and separate public projections are described below; these owner DTOs and revision ETags remain private.

### Current playthrough lifecycle

A user can have **one current run per game**, either active or paused, and can play multiple different games at once. Historical entries may describe other active/paused experiences without controlling present status. A library entry may also use any status without a current run, supporting lightweight tracking with no invented history. `current_playthrough_id` is null in that case.

`POST /api/me/playthroughs/start` accepts:

```json
{"igdb_id":42,"started_on":"2026-09-09","platform":"PC"}
```

Only `igdb_id` is required. Start creates a fresh UUID with `state: "active"` and `is_current: true`, and creates/updates library membership to `playing` in the same transaction. A current active **or paused** run causes `409 lifecycle_conflict`; finish or abandon it before starting a replay. Previous history, ratings, and reviews are untouched. Unknown games use the existing import path; cached games need no successful upstream call. Catalog import can persist independently if a later personal write fails, but personal history and library status always commit together.

`POST /api/me/playthroughs/{id}/lifecycle` requires the current run's `If-Match` and accepts:

```json
{"action":"complete","finished_on":"2026-09-09"}
```

| Action | Allowed source state | Result state | Library status | Remains current |
| --- | --- | --- | --- | --- |
| `pause` | active | paused | on_hold | yes |
| `resume` | paused | active | playing | yes |
| `complete` | active or paused | completed | completed | no |
| `abandon` | active or paused | abandoned | dropped | no |

`finished_on` is optional/null for complete/abandon and must be absent/null for pause/resume. Dates never default to today. A run can stay active indefinitely and carry a rating/review before it ends. Lifecycle actions leave review, rating, platform, `started_on`, and `presentation_on` untouched. Edit dates through PUT/PATCH with revision protection; dating a review still requires an explicit `presentation_on`, independent of the end date.

Actions on a historical/detached run or an invalid transition return `409 lifecycle_conflict`; stale revisions return `412 precondition_failed` first. Unknown action/state values and client-written `is_current` are rejected. Lifecycle responses return the updated owner playthrough and its ETag; start returns 201 and transitions return 200. Read the library entry to obtain its current projection.

Backfilling or correcting historical state never changes current status, even while a replay is active. PUT/PATCH on a current run can edit dates/content but cannot change its state (including clearing it); use lifecycle actions. Removing library membership atomically detaches the current run, increments its revision, and preserves its state, dates, rating, review, and history. Re-adding membership does not adopt a historical run. Start creates a new current experience. Deleting a current playthrough leaves library status as explicit lightweight tracking and clears the derived current-run reference.

All current-state mutations serialize per authenticated user/game with a transaction-scoped advisory lock, acquired before playthrough row locks. Starts, lifecycle actions, library status saves/removal, and playthrough edits/deletion follow this protocol. A database partial unique index additionally prevents two current runs; check constraints require a current run to be active/paused and prevent end dates on open runs. Competing revisions cannot silently overwrite one another. Concurrent status-only operations use last committed intent; when a current run exists, conflicting status saves fail. Removing membership and starting a run concurrently execute in lock order: the final result is either detached history without membership, or a new coherent current run with membership.

Existing runs migrate with `state: null` and `is_current: false`, preserving dates, content, revisions, and private timestamps. An end date or existing library status is never treated as evidence of completion. No sharing behavior changes. Feature 12 will add retry keys: start currently has no replayable idempotency response; after an uncertain response, read the library/current run before starting again. Transition retries with an old revision return 412.

### Historical chronology policy

The diary sorts by `presentation_on DESC NULLS LAST, id DESC`. The library's derived rating uses that same order over the owner's rated entries only. Dates and UUIDs determine the entire order; actual creation/update times and revisions never break ties. UUIDs are random v4 identifiers, so tie order is stable but has no chronological meaning.

- A review-only entry, rating-only entry, or playthrough summary uses its explicitly chosen `presentation_on`. Rating and review on the same entry share this date.
- A backfilled playthrough appears on that chosen day even if it was recorded today. Play start/end dates do not supply or override it. A caller may explicitly send the same day as a play date when that is the owner's intent.
- A null date means unknown. Undated entries appear after all dated entries, including when play dates exist. Display them as undated, without a guessed day or year. A date-filtered view must exclude them; an unfiltered view may show an undated group.
- Text, rating, platform, play-date, and visibility changes preserve position. Editing `presentation_on` moves the entry; clearing it moves the entry into the undated group. A rating change can affect rating selection by adding/removing a rated candidate, but does not change its position.
- Dated ratings outrank undated ratings. If every rated entry is undated, the highest UUID supplies a deterministic representative rating; it must not be described as the most recently played or recorded rating. Same-day rated ties use the same UUID rule. `rating_playthrough_id` identifies the selected source so the owner can inspect or date it. An unrated entry never suppresses a rating.

`presentation_on` is a PostgreSQL `DATE` / Rust `NaiveDate`, serialized as a calendar-date string, with no time or time zone. The API accepts no time-zone companion and performs no UTC conversion. Clients must preserve the chosen day rather than parse it as an instant and shift it into a local zone. Actual `created_at` and `updated_at` remain UTC instants for owner-side bookkeeping, and date edits use the existing revision and `If-Match` protection.

The presentation-date migration leaves every existing `presentation_on` null, even when start/end dates are present. It changes no stored play dates, content, timestamps, UUIDs, or revisions. Owners can date existing entries in place. The change deliberately removes recording-time and play-date fallbacks from diary ordering and rating selection.

### Contract for future public surfaces (feature 4 onward)

Personal content starts private. The authenticated `Playthrough` DTO and its revision `ETag` are owner-only contracts. Public item endpoints use separate projections; future sections and feeds must also use these safe projections, never owner DTOs or tags.

Notes use an explicit nullable historical day, ordered descending with nulls last, then a stable random identifier. Future list/summary events must follow the same policy. Profiles and feeds must use the same policy. A feed spanning resource kinds needs globally unique random item IDs (or a stable kind discriminator as a final collision tie-breaker), with one identity per underlying item. Publishing or editing an item must not manufacture a current-day event. Unknown dates remain undated in unfiltered lists and do not acquire a year in date-filtered statistics. Play/completion statistics may use explicitly supplied play/milestone dates for the metric they describe; a review date is not proof of play or completion.

A public review or rating may expose its chosen `presentation_on` but must not fall back to private play dates. Playthrough dates, platform, status, other ratings, and notes require their own visibility authorization. Public rating selection must apply the same ordering **only after** filtering to eligible public ratings from enabled public profiles; a private rating must not affect the value, source, ordering, or count.

Public bodies, related resources, previews, pagination metadata, ordering, counts, and headers must omit actual recording/edit/publication timestamps, revisions, edit histories and edited badges. Do not emit timestamp-based `Last-Modified`, revision `ETag`s, or cursors that encode bookkeeping. Any future public cache validator must be opaque, and revocation must invalidate or revalidate cached content. HTTP transport time is not a content historical date. Anonymous reads and owner visitor previews must share these projections and chronology rules.

The migration converts each previous library rating and/or review into one undated playthrough, retaining the original recording/edit timestamps. It creates no playthrough for a status-only entry and invents no play dates or platform.

All `/api/me` library and playthrough routes use the authenticated session as owner. Reading or updating another user's playthrough returns 404; unconditional deletion is a no-op 204. History starts private. Explicitly published items are served separately under `/api/public`; public profiles and their sections apply the same permissions.

## Pagination

`limit` defaults to 20 and must be 1–50. `offset` defaults to zero and is capped at 10000. `q`, when supplied, must contain 1–100 characters without control characters. Local search matches case-insensitive name substrings, with `%` and `_` treated as ordinary characters rather than wildcards, over games stored here — those stored through library saves, playthrough creation, or explicit imports — while catalog search queries all of IGDB by relevance. The `status` filter applies to library listing.

Local game listings return:

```json
{"data":[],"limit":20,"offset":0,"has_more":false}
```

Local games sort by lowercase name then internal UUID. Library, playthrough, note, and public-section collections use the browsing contract below. A game with no own playthroughs returns an empty page, even if it is not stored locally.

Catalog search returns `{data,limit,offset,next_offset}`, where `next_offset` is null on the last page. A full upstream page sets the next offset (until the offset cap); it does not guarantee another result exists. No total-count query is made. Being read-only, it is safe to retry and cache. A successful import returns a single stored game and HTTP 200 whether newly created or refreshed.

## Library and diary browsing

Owner library (`/api/me/library`), experiences (`/api/me/playthroughs`), notes
(`/api/me/diary` and `/api/me/playthroughs/{id}/notes`), public profile sections,
and visitor-preview sections return `BrowsePage<T>`:

```json
{"data":[],"limit":20,"offset":0,"has_more":false,"total":0,"next_cursor":null,"status_counts":null}
```

`total` counts all matching records **before** offset/cursor pagination, including
when the requested page is empty. Rows, totals, library status facets, and public
profile authorization use one read-only repeatable-read snapshot per request.
`has_more` comes from fetching one extra matching row. All filters combine with
AND. Unknown query parameters, unsupported filters/sorts, malformed dates,
nonpositive game IDs, and invalid bounds return 400.

| Filter | Meaning and availability |
| --- | --- |
| `q` | Literal case-insensitive game-name substring on every collection; trimmed, 1–100 characters, no controls. `%`, `_`, and backslash are literal. |
| `igdb_id` | Positive exact game ID on every collection. |
| `year` | Calendar year 1–9999 of `presentation_on`, on dated collections. |
| `from_on`, `to_on` | Inclusive chosen-date range, either or both bounds; if both, start must not exceed end. Cannot combine with `year`. |
| `undated` | `true` selects null chosen dates, `false` selects known dates; omission includes both. `true` cannot combine with year/range. |
| `state` | Exact `active`, `paused`, `completed`, or `abandoned` experience state; owner experiences/notes and public summaries only. Unknown states do not match. |
| `has_rating`, `has_review` | `true`/`false` selects presence/absence on owner experiences, their notes' parent experience, or public summaries. `has_rating` also applies to library derived ratings. |
| `text` | Literal case-insensitive substring of note text or public review text, with the same validation/escaping as `q`. |
| `status` | Exact library status, on owner/public library and current-games sections only. |

Date filters always use the item's own chosen day, never its parent, play dates,
or recording timestamps. Unknown dates do not match a year/range. No timezone
conversion occurs. For example:

```http
GET /api/me/playthroughs?year=2020&has_review=true
GET /api/me/playthroughs?state=paused&q=halo
GET /api/me/playthroughs?state=completed&from_on=2020-01-01&to_on=2020-12-31
GET /api/me/diary?year=2020&text=boss&has_rating=true
GET /api/public/profiles/{id}/reviews?year=2020&q=halo
```

Public summary presence filters join only independently shared ratings/reviews.
A private component behaves exactly like an absent one. Public note, rating,
and review collections do not accept parent-state or component-presence filters;
these could otherwise disclose private parent relationships. Public notes remain
searchable by their own text/date/game even when their parent summary is private.
Library ratings and recent-play ordering use only public inputs, including in
owner preview. Private rows cannot affect counts, cursors, `has_more`, or ordering.
Profile-header counts remain unfiltered section totals; section `page.total`
reflects its query. All public and preview responses retain `no-store` and omit
revision/date validators.

### Library sorting and status facets

`sort` accepts the following fixed orders; `title` is the default:

| Sort | Order |
| --- | --- |
| `title` | Lowercase title ascending, IGDB ID ascending. |
| `rating` | Derived rating descending, nulls last, then title and IGDB ID. |
| `date_added` | Membership `created_at` descending, then title and IGDB ID; **owner library only**. Removing and re-adding membership starts a new date. |
| `recently_played` | Latest explicitly recorded play day descending, nulls last, then title and IGDB ID. |

One membership per game makes the final IGDB-ID tie unique. Title comparisons
use the database's collation consistently. `date_added` is rejected publicly
because it would disclose recording chronology, even through ordering alone.
Owner rows are `{game,entry,last_played_on}`; the additional nullable day explains
the recent-play sort. Public item DTOs remain unchanged.

**Recorded play activity** currently means an explicit `started_on` or
`finished_on` on a playthrough. `last_played_on` is their greatest known day across
that owner's runs for the game. An end date records the last played day regardless
of completion/abandonment outcome. A run with neither day contributes nothing.
Public sorting uses only independently public summaries' play dates. Backfilling,
correcting, clearing or deleting these dates deliberately updates the order.
Review/rating presentation dates, review/note edits, profile edits, visibility
write times, library status changes, and undated lifecycle actions never supply
play activity. No automatic “today” date is inferred. An ongoing game with only
a start day keeps that day until another explicit play day is recorded; this is
recorded history, not a claim that every session has been logged.

Library `status_counts` contains `backlog`, `playing`, `completed`, `on_hold`,
and `dropped`, including zeroes. These facets obey all filters **except `status`**
and ignore pagination, letting a client display alternative status tabs. `total`
also obeys `status`. With no status filter, the five counts sum to `total`.
The `current_games` section remains restricted to playing entries even when
computing facets. Other collections return `status_counts: null`.

### Pagination under historical edits

Dated collections order by `presentation_on DESC NULLS LAST,id DESC`. Use
`limit` (1–50, default 20), then pass `next_cursor` as URL-encoded `after` with the
same collection and filters. A null cursor means no further matching row in that
request's snapshot. Cursors encode only the last returned chosen date (or an
unknown-date marker) and random item UUID; they are value boundaries, not access
tokens or references requiring the anchor row to survive. They contain no
recording times, revisions, publication times, or private row counts. Modified
cursors never bypass current ownership or visibility checks.

Cursor paging continues strictly after that boundary. Insertions before it and
text-only edits do not shift subsequent pages. A new backfilled item after the
boundary is eligible on a later page; one before it requires a refresh from the
start. Deleting or revoking the anchor does not invalidate the cursor. All
subsequent reads reapply current visibility. Moving an item's chosen date across
a previously traversed boundary can still make it appear again or be missed;
refresh after historical date edits or filter changes. This is live traversal,
not a retained multi-request snapshot. Totals describe each request's current
matching set, not just the remainder after the cursor.

`offset` (0–10000, default 0) remains available for jumping to a bounded position;
it cannot be nonzero with `after`. Dated offset pages also return a continuation
cursor. Library browsing uses bounded offset paging and always returns
`next_cursor: null`; `after` is rejected there. Its title/rating/date/play-day
sorts are deterministic, but membership, title, rating, play-date or visibility
changes can shift pages between requests. Reload the library from the first
page after such edits. Catalog/local-game pagination is unchanged.

## Errors

```json
{"error":{"code":"validation_error","message":"rating must be an integer from 1 to 10, or null"}}
```

Application error codes are exactly `validation_error`, `invalid_json`, `not_found`, `method_not_allowed`, `precondition_required`, `precondition_failed`, `lifecycle_conflict`, `handle_unavailable`, `favorites_exists`, `request_rejected`, `internal_error`, `database_unavailable`, `igdb_not_configured`, `igdb_unavailable`, `igdb_rate_limited`, `catalog_busy` — a closed set, published as a TypeScript union and checked against this list by `tests/contract.rs`. Validation is generally 400; malformed JSON/schema/content type retains Axum's 400/422/415 status; oversized bodies return 413.

### Field-level errors

When a request body is rejected because of specific fields, the error carries a `fields` array naming them:

```json
{"error":{"code":"validation_error","message":"2 fields are invalid","fields":[
  {"field":"rating","code":"out_of_range","message":"rating must be an integer from 1 to 10, or null"},
  {"field":"finished_on","code":"before_start","message":"finished_on must be on or after started_on"}
]}}
```

`fields` is **absent**, not empty, when no individual field is at fault — a bad path segment, a bad query parameter, a missing resource, or JSON that never parsed. Clients should branch on the presence of `fields` rather than on the code, because the same array arrives from two places: our own validation (`400 validation_error`) and the JSON schema check that runs before a handler (`422 invalid_json`, or `400 invalid_json` for a malformed document, which names no field). Both statuses and both codes are unchanged from before `fields` existed; the array is purely additive.

Field codes are `required`, `out_of_range`, `too_long`, `invalid_format`, `before_start`, and `unknown_field`. `field` is the JSON name as sent. Every failing field of one submission is reported together, so a form marks everything it got wrong in a single round trip; `message` at the top level summarizes for a client that renders no per-field detail.

Cross-field rules are attributed to the constrained field: a finish date before its start is reported against `finished_on`, which is the control a date picker can mark. Body routes report fields; query parameters and path segments do not, because no control owns them. A playthrough edit without `If-Match` returns 428 and one whose precondition no longer holds returns 412. Upstream failures return 502, unavailable/configuration/rate-limited upstreams return 503, and local catalog contention returns 429 with `Retry-After: 1`.

### Authentication codes on application routes

The list above is what the *application* emits. It is not everything an `/api` route can return. Doorman's session and CSRF layers wrap the whole router, so they reject before a handler runs and answer with their own codes in the same envelope:

| Status | Code | When |
| --- | --- | --- |
| 401 | `session_not_found` | No session cookie or bearer token, or one that no longer validates |
| 401 | `reauth_required` | The session is valid; this action needs a fresh passkey assertion |
| 403 | `csrf_origin_mismatch` | A cookie-authenticated write with a missing or foreign `Origin`/`Referer` |
| 403 | `account_disabled` | The account exists but is not active |

**An expired session reports `session_not_found`, not `session_expired`.** The session layer attaches no context for a session it could not validate, so the extractor behind every authenticated route cannot tell "expired" from "never existed". `session_expired` exists, but only `/auth` routes that consult the session manager directly can produce it. A client that branches only on `session_expired` will never fire; branch on both, or use `isSignedOut` from the contract, which does.

These codes are published as the `AuthCode` union in `contract/errors.ts`, checked against Doorman's own `classify` at the pinned revision by `tests/contract.rs`. `ApiError.code` and `AuthError.code` are both typed `AnyErrorCode`, the union of that set with this application's `ErrorCode` and the browser-side ceremony codes below, because all three genuinely arrive at the same `catch`.

**Never branch on the status alone.** `session_not_found` and `reauth_required` are both 401 and mean opposite things: one requires sending the user to sign in, the other requires keeping them exactly where they are and asking for a passkey. Treating the second as the first discards the user's work at the moment the server asked them to confirm their identity.

### Browser-side passkey codes

`navigator.credentials.create()` and `.get()` reject with a `DOMException`; they do not resolve `null`. These never come from the server, and `contract/auth.ts` translates them so a caller handles them in the same place as everything else: `passkey_cancelled` (dismissed or timed out — WebAuthn reports those identically), `passkey_already_registered` (this authenticator already holds a credential for the account), `passkey_unsupported` (no WebAuthn, or an insecure context), and `passkey_origin_mismatch` (the page's origin does not match the relying-party ID). A cancelled prompt is an ordinary outcome and usually deserves no error message at all.

`ETag` and `Retry-After` are exposed to the browser through CORS, and `If-Match` is an allowed request header, so a `fetch` may both read a tag and send one back. Every response carries an `x-request-id` for correlation. App/auth responses use `Cache-Control: no-store` to avoid leaking private or auth data through shared browser caches. No database details, upstream response bodies or credentials are returned in app errors.

## Generated types and client

`contract/` publishes this contract as TypeScript. `contract/types` is generated from the Rust structs that serialize these responses, so no field name is declared twice and a change here cannot silently diverge from the server. `contract/client.ts` is a hand-written route table over those types, and `contract/auth.ts` is the browser adapter for the Doorman flows below. See [contract/README.md](../contract/README.md) for how each half is kept honest, and prefer them over the raw `fetch` calls shown next.

## SolidJS browser integration

```ts
const API = "http://localhost:3000";
const response = await fetch(`${API}/api/me`, { credentials: "include" });
const me = await response.json();

await fetch(`${API}/api/me/library/${game.igdb_id}`, {
  method: "PUT",
  credentials: "include",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ status: "playing" }),
});

// Editing one field of a playthrough, refusing to overwrite a newer revision.
const saved = await fetch(`${API}/api/me/playthroughs/${playthrough.id}`, {
  method: "PATCH",
  credentials: "include",
  headers: {
    "Content-Type": "application/json",
    "If-Match": `"${playthrough.version}"`,
  },
  body: JSON.stringify({ review: text }),
});
if (saved.status === 412) {
  // Someone else already saved: reload the playthrough and reapply the edit.
}
```

The browser must run at `FRONTEND_ORIGIN`. Cookie writes require a matching Origin or Referer; the browser sends Origin automatically for these fetch requests. Cookie-based curl requests must include an equivalent header. Authenticated curl/tool requests can alternatively send `Authorization: Bearer <doorman-session-token>`; this is an opaque session token, not a JWT.

### Where the API may be deployed

The session cookie is `HttpOnly`, `Path=/`, carries no `Domain`, and is `SameSite=Lax` by default. `Lax` is what constrains the deployment: the cookie is set by the API and has to be attached to `fetch` calls the SPA makes from `FRONTEND_ORIGIN`, and under `Lax` a browser only does that when the two are **same-site** — a comparison of registrable domains, ignoring scheme and port.

Local development never shows the problem. `localhost:5173` and `localhost:3000` differ only by port, so they are same-site and the cookie flows. `gameboxd.example` with an API on `api.gameboxd.example` is also same-site and equally fine. An API on an unrelated domain is **not**, and the browser will silently drop the session cookie on every request — a login that appears to succeed and then behaves as though it never happened, with nothing in the server logs.

Set `COOKIE_SAME_SITE=none` for that topology. It requires an HTTPS `FRONTEND_ORIGIN`, because a browser ignores a `SameSite=None` cookie that is not also `Secure`; startup refuses the combination rather than letting it fail in the browser. Cross-site cookies are still protected against forgery by the Origin/Referer check above, which is independent of `SameSite`. Prefer putting the API on a subdomain of the frontend and leaving this alone.

## Doorman routes and passkey flows

Mounted unchanged at `/auth`; authoritative DTOs and flow behavior come from the [pinned Doorman source](https://codeberg.org/EddieBrunstedt/Doorman/src/commit/17bb0dcd509de6ca3bfd2742d3c9e422c92b4597/crates/axum/src/dto.rs).

- Signup: `POST /auth/signup/request` with `{email}` → email link to SPA `/signup?token=...` → `POST /auth/signup/consume` with `{token}` → WebAuthn registration in browser → `POST /auth/signup/finish` with challenge ID and credential.
- Sign-in: `POST /auth/passkey/authenticate/start` → browser WebAuthn assertion → `POST /auth/passkey/authenticate/finish`.
- Recovery follows `/auth/recovery/request`, `/consume`, `/finish`, with an email link to the SPA `/recovery` screen and passkey enrollment.
- Passkeys: `GET /auth/passkey`, registration start/finish, rename and removal via `/auth/passkey/{credential_id}`.
- Sessions: `GET /auth/sessions`, `POST /auth/logout`, `POST /auth/logout-all`, and session revocation routes.
- Account: `POST /auth/account/email`, `POST /auth/account/email/confirm`, `DELETE /auth/account`.
- Re-authentication: `POST /auth/reauth/start`; provide the resulting fresh assertion in the protected action's `reauthentication` field according to Doorman's DTOs.

Email links only start signup/recovery; they do not create sessions on their own. Successful passkey finish sets the HttpOnly session cookie. Email changes, passkey removal and account deletion require re-authentication; recovery cooldowns also apply. Render frontend screens for `/signup`, `/recovery` and `/account/email`, consume their tokens once, and remove tokens from browser history after reading them. Use the [Doorman browser helper reference](https://codeberg.org/EddieBrunstedt/Doorman/src/commit/17bb0dcd509de6ca3bfd2742d3c9e422c92b4597/examples/web-client) to serialize WebAuthn ArrayBuffers correctly.


## Per-item visibility and public representations

All personal content starts private, including existing records after migration,
new runs/replays, new notes under public runs, and newly re-added library entries.
Creation and ordinary content endpoints do not accept visibility fields. Publish
with a separate explicit visibility write after reading the saved resource.
Changing display name/bio, library status, lifecycle state, note text, or review
text preserves that resource's existing choice. New children never inherit it.
Clearing a rating or review to null resets that component's visibility to private;
adding its replacement later requires a new publication choice. Changing an
existing published rating/review is an edit to that public item.

The profile's `public_enabled` switch is a separate gate. Initialize/read the
profile with `GET /api/me`, then send:

```http
PUT /api/me/publication
If-Match: "1"
Content-Type: application/json

{"public_enabled":true}
```

Disabling it suppresses **all** public items, even direct links, without changing
stored item choices, content, or item revisions. Re-enabling restores the selected
presentation. The display name is public author attribution when enabled; neither
email nor the Doorman user ID is public identity. Stable public profile identity,
sections and owner preview are described below. Lists receive visibility in feature 7.

Visibility bodies:

- Library entry and note: `{"visibility":"public"}` or `{"visibility":"private"}`.
- Playthrough: `{"summary_visibility":"private","rating_visibility":"private","review_visibility":"public"}`.
  All three fields are required, independently `private` or `public`. A public
  rating/review requires existing non-null content; otherwise `400 validation_error`
  names the offending visibility field. Validation is atomic.

All four control endpoints require `If-Match` using the resource's owner `version`.
They return 200 and the updated owner profile, library entry, playthrough, or note.
Playthrough/note responses also carry their owner revision ETag. Missing
preconditions return 428; stale revisions return 412. Existing strong tag/list/`*`
syntax applies; clients should send an exact revision to detect concurrent edits.
Malformed enum values, null, missing fields, and unknown fields are rejected.
Foreign resources return the same 404 as absent resources before checking their
revision; anonymous owner operations require authentication. Profile publication
always targets the caller's own profile. No-op publication writes preserve revision
and timestamps. Content and visibility writes share the playthrough/note revision;
library and profile rows now also have database-maintained revisions. Existing
status/profile-text operations preserve publication choices and advance those
revisions when they write. Library publication follows the lifecycle owner/game
lock protocol; note publication follows parent-before-note locking.

Public routes have dedicated types, with this explicit field allowlist:

| Type | Fields besides `id`, `author_id`, `author_name`, and `game` |
| --- | --- |
| `PublicLibraryEntry` | `status`, nullable `rating`, nullable `rating_id` |
| `PublicPlaythrough` | nullable `presentation_on`, `state`, `started_on`, `finished_on`, `platform` |
| `PublicRating` | nullable `presentation_on`, non-null `rating` |
| `PublicReview` | nullable `presentation_on`, non-null `review` |
| `PublicNote` | nullable `presentation_on`, non-null `text` |

`game` is `PublicGame`: `igdb_id`, `game_name`, `game_slug`, nullable `cover_url`,
read entirely from local storage without contacting or refreshing IGDB. The
library public UUID is independent of authentication identity and game identity;
it remains stable until membership is deleted. Re-adding membership creates a new
private entry with a different UUID. Other item IDs are their existing random UUIDs.
Rating and review IDs coincide with the underlying run, but the resource kind is
part of the URL/identity; knowledge of that UUID never grants another projection.

Publishing a summary explicitly shares its state, platform and chosen play dates;
it shares no rating, review, notes, or current-run/library relationship. Publishing
only a review shares its text, game and chosen author/day attribution. Notes share
no parent ID or parent details. A public note remains available with a private
summary; deleting the parent removes the note. Library removal preserves public
history and notes. Independently public components remain available when another
component is made private. Account deletion removes all personal public items.

### Reusable visibility-aware reads

The PostgreSQL views `public_library_entries`, `public_playthroughs`,
`public_ratings`, `public_reviews`, and `public_notes` are the common eligibility
boundary. Each includes the profile gate and only its explicit item choice.
Direct handlers read these views into dedicated public DTOs, never owner DTOs.
Related resources, search and feeds must use these gated views or an equivalent
authorized snapshot, applying filtering, ordering, pagination and aggregation
**after** visibility. Profile sections and previews use the shared `selected_*`
item projections after resolving their profile scope in a single read-only
repeatable-read transaction. Only an authenticated session owner preview may
resolve a disabled profile; `selected_*` alone does not authorize public reads. Their `user_id` and `game_id` columns are server-only join keys and
must never be serialized. Views contain no write/edit timestamps or revisions.
These are application query boundaries, not database role isolation/RLS.

Dated collections use `presentation_on DESC NULLS LAST, id DESC`, with no
recording/edit/publication-time fallback. The same-day and unknown-day tie break is
a random UUID, not chronological evidence. Example section read:

```sql
SELECT id,author_name,igdb_id,game_name,game_slug,cover_url,presentation_on,review
FROM public_reviews WHERE user_id=$1
ORDER BY presentation_on DESC NULLS LAST,id DESC LIMIT $2 OFFSET $3;
```

Counts use the identical filtered view and scope. The public library rating is
derived **only** from `public_ratings` for the same owner/game, using that historical
ordering. Private ratings cannot change its value, source ID, ordering, counts or
page boundaries. A public rating does not require library membership or a public
summary. If all eligible ratings are undated, the highest UUID provides the
representative without claiming recency. With none eligible, both public library
rating fields are null. Library sections must not sort using owner timestamps;
public sections support title, public rating, and explicitly shared play-date sorting.

### Revocation and caching

Every public-content read checks current database visibility in one statement or one read-only repeatable-read transaction.
There is no stored public response, materialized feed, or positive/negative item
cache to invalidate. The application sends `Cache-Control: no-store` on successes,
404s, errors and HEAD responses. Public-content middleware strips ETag and
Last-Modified, and these routes do not return 304 or process conditional GETs from
owner revisions. Generic HTTP Date/request IDs describe the request, not content
history. Public DTOs carry no creation/edit/publication timestamps, revisions,
visibility flags or edit badges; public local catalog reads also omit import and
refresh timestamps.

After a privacy change or deletion commits, subsequent reads return the same
`404 not_found` as an unknown item, including for an authenticated owner on a
public URL. Already delivered content and reads whose database snapshot preceded
the change cannot be recalled. Reverse proxies/CDNs must honor `no-store` and must
not override it with stale serving. Clients must refetch public content for a new
view and evict displayed/cached results on a known revocation; an owner's stored
choices are separate from visitor data. Any future response/feed cache requires
synchronous visibility revalidation before serving, including profile disabling,
item revocation, deletion, and later moderation restrictions. Do not introduce
TTL-only revocation or validators derived from private bookkeeping.


## Public profiles, stable links and visitor preview

### Identity and handle policy

`Profile.public_id` is a database-generated random UUID, independent of Doorman
identity and email. `GET /api/public/profiles/{public_id}` is the canonical stable
profile URL. It works without a handle when publication is enabled. Profile UUIDs
and direct item URLs survive handle and display-name changes; deleting an account
ends those identities. Nothing redirects a deleted identity to a new account.

Initialize/read the owner profile via `GET /api/me`, then claim or rename:

```http
PUT /api/me/handle
If-Match: "1"
Content-Type: application/json

{"handle":"Player_42"}
```

Handles contain 3–30 ASCII letters, digits or underscores and must start with a
letter. Input is normalized to lowercase (`player_42`); whitespace is rejected,
not trimmed. The namespace is separate from API route names, so there are no
reserved route words. Handles are optional until first claimed; this operation
requires a string and changes only the caller's handle. It returns the updated
owner `Profile`. Invalid format returns a field error; unavailable names return
`409 handle_unavailable`. Missing/stale revision preconditions return 428/412.
Claims and renames are transactional and unique across concurrent requests;
publication changes share the profile revision. Reclaiming the current canonical
handle is a no-op and preserves revision/timestamps.

`GET /api/public/handles/{handle}` case-insensitively resolves a **current** handle
and returns the same `PublicProfile` as its UUID route. Disabled, unknown, invalid
and retired handles return indistinguishable 404s. A rename retires the old handle
without a redirect or public alias history. Retired handles remain reserved to
the original profile and may be restored by that owner. Account deletion erases
the reservation's owner reference but keeps the name unavailable to prevent
impersonation through old links. No public reservation/availability endpoint
exposes claim history. Broader claim throttling is part of the resource controls
planned for feature 12.

### Profile and sections

`PublicProfile` contains exactly `id` (public UUID), nullable canonical `handle`,
`display_name`, nullable `bio`, and `counts`. It exposes no profile gate, private
identity, visibility settings, write/edit times or revisions. Counts are selected
row counts for `current_games`, `library`, `playthroughs`, `ratings`, `reviews`, and
`notes`, plus `lists` (including the favorites list), independently calculated from each section's eligible records. They are
not total private history or counts of unique games across sections.

Existing and new biographies are private by default. To include a bio, explicitly
send `bio_visibility: "public"` with `public_enabled` in
`PUT /api/me/publication`. `"private"` revokes it. Omission/null preserves the
stored choice. Enabling the profile alone never shares an existing bio. A private
bio projects as null, even in visitor preview; an explicitly shared empty bio is
an empty string. Ordinary profile text edits preserve the choice, so editing an
already shared bio updates that public text.

`GET /api/public/profiles/{id}/{section}` accepts these section names:

| Section | Selection and projection | Order |
| --- | --- | --- |
| `current_games` | Public library entries with status `playing`, as `PublicLibraryEntry` | Lowercase game title, IGDB ID ascending by default |
| `library` | All public library entries, as `PublicLibraryEntry` | Same title order; optional rating/recent-play sort |
| `playthroughs` | Independently public run summaries, as `PublicPlaythrough` | Chosen day descending, unknown last, UUID descending |
| `ratings` | Independently public ratings, as `PublicRating` | Same historical order |
| `reviews` | Independently public reviews, as `PublicReview` | Same historical order |
| `notes` | Independently public diary notes, as `PublicNote` | Same historical order |
| `lists` | Public custom/favorites lists, as `PublicList` | Lowercase list title, UUID ascending |

Current games include lightweight status tracking; the section does not expose or
infer private current-playthrough relationships. A paused/on-hold entry is in the
library section but not current games. Library ratings use only independently
selected public ratings, including during disabled-profile preview. Reviews and
notes can appear without a public library entry or parent summary. Every item
carries its `author_id` (public profile UUID) and display-name attribution.
Game-based items include local game ID/title/slug/cover context; list sections
return summaries whose ordered game context is available at the list detail URL. No profile read contacts or refreshes IGDB.

Responses are a discriminated `{section,page}` envelope. `page` has
`BrowsePage<T>` with filtered totals, library facets, and dated continuation
cursors, as described in [browsing](#library-and-diary-browsing). Private rows
affect neither pagination nor counts. Public empty sections return 200 with an empty page; a missing/disabled
profile returns 404 even for an empty section. Unknown section names and invalid
queries return 400.

Offset pagination has deterministic ties but is not a snapshot across separate
requests: inserts, deletion, changed chosen dates, title changes or permission
changes can shift page boundaries. Each request authorizes and reads from one
consistent database snapshot. Text edits and publication times never bump an item
in historical order; unknown dates never fall back to recording or play dates.

### Stable item links and visitor preview

Use `/api/public/reviews/{playthrough_id}` and `/api/public/notes/{note_id}` to
share individual diary items independently of pagination. Existing direct summary,
rating and library URLs remain valid. Detail and section rows have identical
component fields and independent item permissions. A private/missing item or a
disabled profile returns 404 even to a signed-in owner. Revocation or deletion
removes the item from detail, sections and counts on subsequent reads.

`GET /api/me/public-preview` and `GET /api/me/public-preview/{section}` require
an authenticated session and always target that session's owner. They use exactly
the public profile/section DTOs, counts, item choices, ordering and pagination.
For an enabled profile, preview and anonymous reads match for the same database
state. For a disabled profile, preview bypasses **only** the profile gate; private
items and private bio remain hidden. No preview URL token, target user parameter
or public query flag can grant this bypass. Read `/api/me` to label the owner's
publication state; it is intentionally absent from the public presentation.

All public and preview responses, including errors and HEAD, are `no-store` and
omit revision ETags and Last-Modified. Reads already in flight may finish with
their authorized snapshot; subsequent reads reflect revocation. Do not serve
previously fetched public content from a cache after permission changes.


## Favorites, ordered lists and want-to-play

Library membership describes tracking, **not ownership**. `backlog` is the
canonical lightweight **want-to-play** status: it says the user intends to play,
regardless of whether they bought, borrowed, subscribed to, or own the game.
It neither claims ownership nor creates a playthrough. There is no separate
built-in wishlist competing with backlog. Existing statuses/history are not
rewritten. Store ownership, purchases and subscriptions are outside this API.

Lists are independent curation. A custom “Play next” list can order a subset of
intentions, include possible replays of completed games, or contain games with no
library entry. Adding/removing games never sets a status, creates history, or
changes ratings. Removing library membership or playthroughs leaves lists intact;
deleting a list leaves games, history and library entries intact. Account deletion
cascades to its lists and memberships.

Create with `POST /api/me/lists`:

```json
{"kind":"custom","title":"Play next","description":"A few games for autumn."}
```

`kind` defaults to `custom`; `favorites` identifies the owner's single favorites
collection. Favorites use exactly the same ordering, editing and publication
rules as custom lists. Create them explicitly with `kind: "favorites"`, discover
them by `kind` in the owner's list summaries, and edit by their stable UUID.
Concurrent attempts to create favorites produce one 201 and one
`409 favorites_exists`. Edit the existing favorites list after that conflict.
The kind is immutable, while its title/description are editable. Deleting favorites
removes that collection; recreating it creates a new UUID, empty and private.
No favorites or lists are inferred from ratings or existing records.

`title` is required, non-null, nonblank, and at most 120 Unicode characters.
`description` is at most 5,000 Unicode characters; omission becomes an empty
string. Both are plain text preserved verbatim and reject NUL; render as text.
Titles need not be unique. Creation rejects visibility and membership fields;
new lists are always empty and private. Unknown fields and invalid shapes return
422, invalid values return field-specific 400. `PUT /api/me/lists/{id}` takes
`{title,description?}`, replacing only those two fields; omitted description
clears it. It cannot change kind, games, or publication.

`PUT /api/me/lists/{id}/games` performs addition, removal and reorder atomically:

```json
{"igdb_ids":[42,43,44]}
```

The array is the **complete desired membership in order**, limited to 500 distinct
positive IGDB IDs per list. Duplicate, nonpositive, or unstored IDs reject the
entire operation with a field-specific 400; nothing changes. Import unknown games
first with `POST /api/catalog/import/{igdb_id}`. Importing only stores catalog
metadata, never library membership. List writes/read responses use local metadata
and make no IGDB requests. An empty array clears the list. The bounded complete
representation lets clients reconcile membership and order from one snapshot.
Array order is the persisted order; no recording timestamp supplies a tie-breaker.

All PUT and DELETE list operations require `If-Match` with a quoted decimal
revision from `list.version` or the owner detail ETag. Missing preconditions return
428; malformed headers return 400; stale or weak-only tags return 412. Unlike the
older item routes, lists reject `If-Match: *` with 400: edits must name an observed
revision. Multiple strong tags use the existing match-any-tag semantics.
The list's single revision covers metadata, visibility, membership and order.
Real edits increment it once; identical writes preserve revision/timestamps.
All edits and deletion acquire the same list row lock. Conflicting edits cannot
silently discard membership changes, even if one edits publication or the title.
After 412, reread and reconcile; do not blindly retry a stale array with a fresh
revision. An uncertain creation response requires rereading collections before
retrying; general creation idempotency remains future work. Missing/other-owner
IDs return the same 404, including conditional deletion after a prior deletion.

Owner summaries (`GameList`) contain `id`, `kind`, `title`, `description`,
`visibility`, `game_count`, `version`, `created_at`, and `updated_at`. Owner GET
and successful POST/PUT detail responses (`GameListDetail`) are `{list,games}`:
`list` is that summary; `games` is the full ordered array of public game context
(`igdb_id`, `game_name`, `game_slug`, nullable `cover_url`). Detail responses include
the list revision ETag. Games have no membership/status/rating or parent-history
fields. GET reads metadata, counts and members in one repeatable-read snapshot.

`PUT /api/me/lists/{id}/visibility` accepts `{"visibility":"public"}` or
`{"visibility":"private"}`. **Publishing a list explicitly shares its title,
description and entire ordered membership. Later membership additions and text
edits are explicit edits to that public list and become visible immediately when
the profile is enabled.** This never publishes the games' library entries,
playthroughs, ratings, reviews, notes or other lists. Metadata/membership edits
preserve visibility. Because visibility shares the aggregate revision, an edit
based on a private version cannot inadvertently land after concurrent publication.

Public list summaries (`PublicList`) expose only `id`, `kind`, `title`,
`description`, `game_count`, `author_id`, and `author_name`. Direct public detail
`GET /api/public/lists/{id}` returns `PublicListDetail` (`{list,games}`), with exactly
the safe game context above. No internal dates, revisions, visibility, authentication
identity, library membership, status, ratings or playthrough details are exposed.
Public lists are ordered by title, not creation, edit or publication time, and do
not create dated activity events. UUID links survive title and membership edits.

`GET /api/me/lists`, profile section `GET /api/public/profiles/{id}/lists`, and
owner preview section `GET /api/me/public-preview/lists` support only `q` (literal,
case-insensitive **list title** search), `limit` (1–50, default 20), and `offset`
(0–10000, default 0). Other browsing filters/sorts/cursors are rejected with 400.
They use the common `BrowsePage` envelope (profile/preview wrap it in
`{section:"lists",page}`), with matching `total`, `has_more`, null `next_cursor`
and null `status_counts`. Ordering is `lower(title), id` ascending, with stable UUID
ties. Offset pages can shift across calls after insertions, deletions, renames or
permission changes; refresh after edits. Counts and rows within a response share
one authorized snapshot. Profile `counts.lists` counts selected lists including
favorites, independently of pagination/search; `game_count` counts only members of
that explicitly selected list, never library entries or private history.

Direct public reads and public sections require both public list visibility and
an enabled profile. Owner/other-user cookies grant no bypass. The authenticated
owner preview detail (`GET /api/me/public-preview/lists/{id}`) and section use the
same selected projection, bypassing only the owner's disabled profile gate.
Initialize the owner profile through `/api/me`, as for existing preview operations.
A private list is unavailable even in preview. Making a list private, deleting it,
or disabling the profile removes subsequent public access with indistinguishable
404s and removes it from public sections/counts. All public and preview responses,
including errors/HEAD, are `Cache-Control: no-store`, without ETag or Last-Modified.
An already in-flight read can finish from its authorized snapshot; no stale response
cache may continue serving revoked lists.

## Personal statistics and explicitly shared summaries

| Method | Route | Access / result |
| --- | --- | --- |
| GET | `/api/me/statistics` | Session owner's complete `Statistics` |
| GET | `/api/public/profiles/{public_id}/statistics` | Selected public `Statistics`, only with an enabled profile and public summary choice |
| GET | `/api/me/public-preview/statistics` | Session owner's selected public `Statistics`; bypasses only the profile gate |

Statistics publication is a separate choice on the owner `Profile`:
`statistics_visibility: "private" | "public"`, initially **private**, including
for profiles already enabled before migration. Set it through
`PUT /api/me/publication` alongside required `public_enabled`, using the profile's
current `If-Match` revision. Omission or null preserves the summary choice.
No-op writes preserve timestamps/revisions; competing profile/publication edits
use the same profile lock and revision. Enabling a profile does not publish its
summary. Publishing a summary does not publish any underlying private component.
Disabling the profile preserves all choices. Missing, disabled, and unshared
summaries return the same 404, even to a signed-in owner. Preview requires the
summary choice to be public, so select it while the profile is disabled to preview
before enabling. The personal endpoint always remains available to its owner.

All three endpoints accept only `year`, `from_on`, `to_on`, and `undated`:

- `year=2020` selects January 1 through December 31, inclusive (years 1–9999).
  Alternatively use an inclusive date range, with either or both endpoints.
  Reversed ranges, year combined with a range, duplicate/unknown parameters,
  malformed dates and invalid booleans return 400.
- Date filtering uses **`presentation_on`**, the chosen historical day, for both
  run and rating statistics. It never substitutes start/end dates or actual write,
  edit or publication timestamps. A run with a known end date but no chosen
  presentation date remains undated for these statistics, matching diary browsing.
  A yearly completion count therefore means completed runs presented in that year,
  not a claim that their end dates fall within it.
- Without date filters, include dated and undated records. `undated=true` selects
  unknown chosen dates and cannot combine with a year/range; `undated=false`
  selects known dates and may narrow a year/range. Date filters exclude unknown
  dates. Calendar days are not converted through time zones.
- **`current_status_counts` always describes the current library**, unaffected by
  historical filters. There is no reconstructed historical library snapshot.

`Statistics` is a dedicated aggregate response with three fields:
`playthroughs`, `rating_distribution`, and `current_status_counts`. It has no
resource IDs, write/edit timestamps, revisions, generated-at times or history.
The owner/public endpoints share this safe wire shape, but use different inputs.

A **recorded played run** has a non-null state (`active`, `paused`, `completed`,
`abandoned`) or at least one explicit start/end date. An otherwise unknown
review/rating-only entry is not evidence of a run, even if its assessment has a
presentation date. Lightweight library status, notes, list membership and
favorites do not create recorded runs. Unknown-state runs with play dates count
as played, but not completed. Within the eligible, date-filtered run set:

| `playthroughs` field | Definition |
| --- | --- |
| `played_playthroughs` | Number of recorded played runs |
| `unique_games_played` | Distinct games represented by those runs |
| `completed_playthroughs` | Runs with state exactly `completed`; a finish date or abandonment alone never qualifies |
| `unique_completed_games` | Distinct games among completed runs |
| `replay_count` | Sum of `max(run_count_for_game - 1, 0)`; additional recorded runs beyond one per game |
| `replayed_games` | Distinct games with at least two recorded runs |
| `undated_playthroughs` | Recorded runs whose chosen presentation date is unknown, within the selected set |

Two completed runs of one game give two played runs, two completions, one unique
played/completed game, one replay and one replayed game. Replay counts do not
require completion. They are calculated **within the selected period**, without
looking up earlier runs outside it: one run in each of two years contributes one
all-time replay but zero in each year's isolated statistics. There is no claim
about which undated or tied run was chronologically first. Hidden runs never
classify a public run as a replay. Deleting/correcting a run deliberately changes
these counts.

`rating_distribution` always contains ten `{rating, count}` buckets in ascending
1–10 order, including zero buckets. Each eligible non-null rating contributes
once, including rating-only entries and repeat assessments of the same game.
It is an **entry distribution**, not the library's latest-rating-per-game rule.
No rating is inferred from an unrated run. `current_status_counts` contains
`backlog`, `playing`, `completed`, `on_hold`, and `dropped`, counting current
library entries per status. A `completed` library status is lightweight tracking
and is independent of recorded playthrough completions; no status asserts ownership.
Empty accounts return zero counts and all ten zero rating buckets.

Personal inputs are the owner's recorded runs, ratings and library entries.
Public/preview inputs are **only** `selected_playthroughs`, `selected_ratings`,
and `selected_library_entries`, after authorizing the profile and summary choice.
A public rating contributes without publishing its run summary; a public summary
contributes no private rating. A public review/note/list contributes neither a run
nor a rating by itself. Counts never include private runs, private library statuses,
private ratings, or effects of private rows on deduplication/replay classification.

Each response authorizes and calculates all metrics in one read-only repeatable-read
snapshot. There are no materialized counters or caches: historical edits, deletion,
component revocation, profile disabling and summary revocation take effect on
subsequent reads. A read already in flight may complete using its earlier authorized
snapshot, as with other public views. Every response (including errors and HEAD) is
`Cache-Control: no-store`; public/preview responses omit ETag and Last-Modified.
No provider request is needed. Refetch after changes; a previously delivered response
cannot be withdrawn from a visitor's memory.

## Private following and chronological gaming-life feed

All social routes require authentication. A reader can keep their own profile
private (or uninitialized). Targets use canonical **public profile UUIDs**, never
handles, email addresses, or authentication IDs.

| Method and route | Behavior |
| --- | --- |
| `PUT /api/me/following/{public_id}` | Follow an enabled public profile. Bodyless; returns 204, including an existing relationship. Self-follow returns 400. Disabled or missing targets return the same 404. |
| `DELETE /api/me/following/{public_id}` | Unfollow. Bodyless, idempotent 204 even if the target is disabled, deleted, unknown, or not followed. |
| `GET /api/me/following` | Page the session owner's currently available followed profiles. |
| `GET /api/me/feed` | Page live eligible public components from followed profiles. |

Following writes set the desired relationship, so retries cannot duplicate edges;
no `If-Match` is needed. Concurrent operations on a target serialize, including
follow/unfollow and profile publication/deletion. The final serialized operation
wins; clients should serialize their own intent and reread after an uncertain
response. No notification or public activity is created by a follow.

Relationships, following lists, and follower lists/counts are private in v1.
There is no API for inspecting another person's social graph or for notifying an
author about followers. Following responses contain only `public_id`,
`display_name`, and nullable current `handle`. Disabled targets disappear from
this listing and the feed, without removing the stored relationship. Reenabling
the profile restores eligible content under its original chosen dates. A known
UUID can still be unfollowed while disabled. Account deletion cascades incoming
and outgoing relationships; recreating an account does not restore them.

Both collections accept only `limit` (default 20, range 1–50) and optional
`after`. Unsupported or malformed parameters return 400. Both return
`SocialPage<T>`: `{data, limit, has_more, next_cursor}`. There is no total count,
recording timestamp, revision, publication timestamp, or social notification
metadata. `next_cursor` is null at the end. Each read, including its extra row
used for `has_more`, uses one database statement and a consistent snapshot of
relationships, profile gates and content. The following list sorts by public
UUID ascending; its cursor is the last returned UUID. This stable ordering
reveals no follow chronology and is unaffected by display name/handle edits.

### Feed kinds, identity and chronology

Each feed row is a discriminated `{kind, item}` object:

| `kind` | `item` representation and eligibility |
| --- | --- |
| `review` | `PublicReview`; independently selected public review text, once per playthrough. |
| `note` | `PublicNote`; independently selected public diary note, even when its parent summary/review/rating is private. |
| `milestone` | `PublicPlaythrough`; independently selected public summary with explicit state `active`, `paused`, `completed`, or `abandoned`. |

A milestone presents the run's **current chosen historical summary**: an active
run indicates starting/playing, a paused run indicates a pause, and completed or
abandoned runs indicate those outcomes. There is one milestone per run, not an
audit record per lifecycle transition. Correcting the state changes that same
item in place. No transition date is inferred from recording time, publication,
or a missing play date. Summaries with unknown state do not generate milestones;
rating-only shares, library statuses, lists, favorites, profile edits and follow
operations do not generate feed items.

Identity is the pair **`(kind, item.id)`**. A run can contribute one milestone
and one independently public review. Its milestone contains no review text or
rating; the review is never repeated through a summary or library projection.
Note IDs have their own kind namespace. IDs persist through text corrections,
state changes, date changes, and unpublishing/republishing. Item payloads match
the corresponding direct public endpoint exactly, including author identity and
local game context; no IGDB availability is required.

Order is `presentation_on DESC NULLS LAST, id DESC, kind DESC`, with numeric kind
tie order `milestone` (2), `note` (1), `review` (0). Thus separate components
sharing a UUID still paginate without loss or duplication. Only the explicit
chosen presentation day is used, including for milestones: start/end dates,
write times, follow times and publication times never supply a fallback.
Unknown dates remain unknown and follow dated items. Calendar dates do not
shift with viewer time zones. Historical text edits, visibility changes and
following today do not imply activity today. A newly shared old entry appears
at its historical position; changing its chosen date deliberately moves it.

The versioned feed cursor encodes only a chosen day (or `unknown`), UUID, and
kind, e.g. `f1:2020-02-29:<uuid>:review`. Treat it as an opaque value and pass it
back as `after`. It is a value boundary, not an authorization token or a stored
snapshot: every continuation checks the current session's follows and current
visibility. Removing/unpublishing/unfollowing the anchor does not invalidate
continuation. Backfills after the boundary can appear on later pages; newly
eligible items before it require a refresh. Date corrections crossing an
already traversed boundary can repeat or skip an item; refresh after such edits
and deduplicate by `(kind, item.id)` when accumulating pages. Unknown-date
continuations stay in the unknown-date section. Cursors must not be interpreted
as evidence that their source item still exists or is accessible.

### Revocation and future blocking

The feed reads gated `public_reviews`, `public_notes`, and
`public_playthroughs` directly, restricted to the reader's live relationship
set. Private siblings, ratings, statuses, parent relationships and internal
bookkeeping cannot affect eligibility, ordering, page boundaries or payloads.
Making an item private, removing its content, deleting a note/playthrough,
unfollowing, disabling the profile, or deleting an account removes affected
items on subsequent reads, including cursor continuations. An in-flight read
may finish with its authorized snapshot. Previously downloaded text cannot be
retracted from a client's memory.

Feed/following responses (including errors and HEAD) are `Cache-Control:
no-store`, without ETag or Last-Modified. Conditional requests do not serve
stale content. There are no push/email notifications or retained activity-event
copies. Feature 10 must extend the relationship write checks and feed source
scope for both directions of blocking, remove existing blocked relationships,
and apply moderation gates through the shared public sources. Blocking and
moderation are not introduced by this feature.

## Blocking, reporting and operator moderation

Safety state is separate from owner visibility choices. All safety routes require
an authenticated session, use the normal CSRF/origin policy, and return
`Cache-Control: no-store`. No report, block or moderation chronology becomes a
public event. Public responses still omit recording/edit timestamps and revisions.

### Blocking

- `PUT /api/me/blocks/{public_id}` blocks an existing profile, including a disabled
  or restricted one, and atomically removes follows in **both** directions.
  It is idempotent and returns 204. Self-blocking is a 400 validation error;
  unknown/deleted targets return 404.
- `DELETE /api/me/blocks/{public_id}` removes only the caller's block and returns
  204, including for already removed/deleted targets. It never restores follows.
  A reciprocal block still applies.
- `GET /api/me/blocks` returns the caller's blocked profile UUIDs only, including
  disabled profiles, ordered by UUID. It reveals no blocker lists, counts or
  chronology to another user. Account deletion cascades relationships.

An active block in either direction prevents following (404), excludes both
accounts from one another's following/feed sources, and makes authenticated
public profile/handle, section, statistics and direct-item reads return 404.
Owners retain their private reads and visitor previews. Pair-scoped database
locks serialize follow/unfollow/block/unblock across both directions and app
processes; reads independently enforce blocks rather than trusting deleted follows.

**Blocking is not privacy.** A logged-out visitor can still read otherwise public
content, as can a different account. Use item visibility or disable the public
profile to remove public access. Reporting remains possible after blocking:
it is a private safety operation, not an interaction delivered to the target.

### Reports and evidence

`POST /api/me/reports` accepts `ReportInput`:

```json
{"kind":"review","target_id":"<public item UUID>","reason":"harassment","details":"Context for the operator"}
```

`kind` is `profile`, `review`, `note` or `list`. `reason` is `spam`, `harassment`,
`hate`, `sexual`, `violence` or `other`. Optional `details` defaults to empty;
it is plain text, at most 2,000 Unicode characters, with no NUL. Only currently
public, unmoderated targets are reportable; unavailable/private/deleted targets
return indistinguishable 404s. Self-reporting returns 400. No attachment, arbitrary
URL fetch, email, notification, or automatic moderation action is performed.

A successful response is 200 `ReportReceipt` (`{"id":"<case UUID>"}`). There is
one case per reporter/kind/target for the lifetime of that reporter account,
including resolved/dismissed cases. Repeated or concurrent submissions return
the same receipt and do not overwrite the first reason, details or evidence.
The public-target check still applies to retries after removal. Each account
may create at most 20 distinct cases per rolling 24 hours; new cases over this
bound return 429 `report_limit`. Retries do not consume quota. Deduplication and
quota checking are serialized in PostgreSQL, including across processes.

Evidence captures the public representation at report time: selected profile
identity/bio, an independently public review/note and game context, or list text
and its ordered public members. It contains no private diary context, library
membership, hidden rating, private biography, actual content timestamps or
revisions. Evidence is a snapshot and can differ from the subsequently edited
content. Reporter details and evidence are untrusted plain text/data; clients
must escape them, never render them as HTML. Evidence is typed as `unknown | null`
in the contract because its object shape depends on the target kind.

There is no user report-read route or report status notification. Other users,
including the reported owner, cannot access cases or learn who reported them.
Operator DTOs also omit reporter/account authentication identifiers. These are
retained privately in storage for deduplication, authorization and audit.

### Operator access and decisions

Operator access requires a live row in `gameboxd.moderation_operators` for the
session's authentication user ID. There are no default operators, email-based
privileges, self-promotion routes or client-provided role claims. Provision and
revoke grants through trusted database administration. For an operator who has
initialized their app profile, an administrator can bind their verified public
UUID as `$1` in either statement:

```sql
INSERT INTO gameboxd.moderation_operators(user_id)
SELECT user_id FROM gameboxd.profiles WHERE public_id=$1
ON CONFLICT DO NOTHING;

DELETE FROM gameboxd.moderation_operators
WHERE user_id=(SELECT user_id FROM gameboxd.profiles WHERE public_id=$1);
```

Every operator operation checks and locks the grant for its transaction;
revocation waits for already authorized operations and takes effect for subsequent
requests, even with an existing session. Anonymous access returns 401; ordinary
sessions return 403 `forbidden`. Operator credentials and database grants must
be limited to trusted administrators.

| Route | Behavior |
| --- | --- |
| `GET /api/operator/reports` | Cases in `created_at,id` ascending order, with status and evidence. |
| `GET /api/operator/reports/{id}` | One case, including its private version and reporting timestamp. |
| `PUT /api/operator/reports/{id}` | Set `status` (`open`, `resolved`, `dismissed`), with `expected_version` and nonblank `reason`. |
| `GET /api/operator/restrictions` | Current restriction states (including restored states), ordered by `kind,target_id`. |
| `PUT /api/operator/restrictions/{kind}/{id}` | Set `active` with `expected_version` and nonblank `reason`. Kinds match report targets. |
| `GET /api/operator/audit` | Private decisions in `created_at,id` ascending order. |
| `GET /api/me/restrictions` | The caller's active `kind,target_id` pairs only; no case evidence or operator reasons. |

All list routes in this section, including blocks and owner restrictions, accept
only `limit` (1–50, default 20) and `offset` (0–10,000, default 0), and return arrays.
These are live bounded offset pages; refresh after mutations. Unknown parameters
are rejected. Operator reasons are plain text, nonblank, at most 2,000 Unicode
characters and contain no NUL.

A new restriction starts at version 0. Send `expected_version: 0` for its first
decision; subsequent decisions require the fetched version. A report starts at
version 1. A successful operator decision increments its version and appends a
private audit row atomically, including explicit reaffirmations of a decision.
Stale/concurrent decisions return 412 `precondition_failed`, with no partial
state or audit write. Resolving a report and restricting content are deliberate
separate operations: resolution never implicitly hides/restores anything.
A lost response requires rereading the state before another decision.

A `review`, `note` or `list` restriction hides just that component. For example,
hiding a review leaves an independently public rating or playthrough summary
eligible. A `profile` restriction suppresses **all** that account's public
content, profile/handle access, statistics and public participation (following
in either direction). Existing follows are retained but ineligible until the
restriction is lifted; blocks, unlike restrictions, remove follows permanently.
Restricted callers attempting to follow receive 403 `moderation_restricted`;
unavailable targets still receive 404.

Restrictions are enforced by the shared selected/public sources, so direct
links, profile sections/counts, rating derivation, filter presence checks,
statistics, feed payloads/page metadata and visitor previews agree. Preview may
bypass only profile publication, never moderation. Hiding a profile makes its
preview unavailable; hiding an item removes it from preview. Subsequent reads
reflect decisions immediately, with no cached public copies. A read already in
progress can complete against its authorized statement/transaction snapshot.

Private owner reads, corrections, deletion and statistics remain available;
moderation never deletes the diary or changes its visibility choices, timestamps
or revision. `GET /api/me/restrictions` explains which selections are suppressed.
Explicit visibility/publication writes selecting restricted public content
return 403 `moderation_restricted`. Owners can turn selections private, but
cannot clear restrictions or make a restricted component public through edits.
Under an account restriction, a playthrough visibility write must select all
components private; to revise profile publication choices, disable the profile.
Private content edits may retain their stored public selection while remaining
suppressed. Restoration makes content eligible only if its owner still selects
it and the profile is enabled; it creates no public chronology event. New copies
have new identities: use an account restriction when item-level moderation is
insufficient to stop repeated public abuse.

### Deletion and retention

Reports and audit records are private operational data. Deleting an item keeps
its already captured public evidence for review; its restriction remains as a
private identity tombstone until account deletion. Deleted targets cannot receive
new restrictions, but their cases can still be resolved. Reporter deletion sets
its private identity reference to null and leaves evidence about the reported
account intact. Deleting the reported account immediately scrubs all its report
evidence/details and operator free-text reasons, removes active/restored
restrictions, and clears private owner references. This happens in the database
account-deletion transaction, including during racing reports/decisions.
Deleting an operator clears its private audit identity reference.

Evidence, reporter details and operator reasons expire 90 days after their
respective report/action creation. Operator reads suppress expired text even
before cleanup; the existing five-minute maintenance loop physically scrubs it.
Minimal case/action metadata (IDs, target kind/UUID, reason category, case status,
private chronology and remaining account references) is retained for deduplication
and operational accountability, without a public API. Do not copy evidence into
logs or external messages. Backup access and retention remain part of the
public-launch operations package; deletion from the live database cannot erase
an older backup or a visitor's previously saved public copy.

## Personal export and round-trip import

All three routes require the importing/exporting owner's session, use the normal
CSRF policy, and return `Cache-Control: no-store`, including errors. There are no
public archive URLs. Treat downloaded files as private: they contain complete
reviews, notes and biography text, including content hidden by moderation.

| Route | Result |
| --- | --- |
| `GET /api/me/export` | Complete `PersonalArchive` JSON, without pagination or truncation |
| `POST /api/me/import/preview` | Read-only `PersonalImportReport`; no provider calls, inserts, reservations or profile initialization |
| `POST /api/me/import` | Revalidates and atomically merges; returns the committed report and ID mappings |

The two POST bodies are `{ "archive": <PersonalArchive>, "profile_policy":
"restore" | "keep" }`. The policy is required. `restore` restores display name
and biography only into a blank private profile (or a new account); it refuses
to overwrite a name, biography, handle or sharing choices. `keep` explicitly
preserves the entire destination profile and imports only the other records.
It supports adding history to an existing account. An archive from the destination
account itself is rejected, preventing self-import from duplicating its history.

### Version 1 archive

The generated [PersonalArchive type](../contract/types/PersonalArchive.ts) and
its referenced `Archive*` types are the exact wire schema. The top-level object
contains only:

- `format: "gameboxd.personal"`, `version: 1`.
- `profile`: source public UUID as `id`, display name, biography, current handle,
  profile publication flag, biography/statistics permissions and `bookkeeping`.
- `games`: each referenced IGDB ID exactly once, with descriptive `name` and
  `slug`. Includes games mentioned only by a list or historical run.
- `library`: record `id`, `igdb_id`, current status, visibility and `bookkeeping`.
- `playthroughs`: record `id`, `igdb_id`, explicit state and `is_current`, rating,
  full review, `presentation_on`, `started_on`, `finished_on`, platform, three
  independent permissions and `bookkeeping`.
- `notes`: record `id`, source `playthrough_id`, full text, `presentation_on`,
  visibility and `bookkeeping`.
- `lists`: record `id`, custom/favorites kind, full title and description,
  ordered `igdb_ids`, visibility and `bookkeeping`. Array order defines membership
  order. Favorites remain the same kind of collection, with at most one per account.

`bookkeeping` explicitly labels actual `created_at`, `updated_at` and `version`
values. These are archival information, not dates of play. The export uses one
read-only repeatable-read snapshot for every collection, including ordered list
membership and game context. It initializes a missing ordinary private profile,
like `/api/me`, before taking the snapshot. It never contacts IGDB. It reads owner
tables, so private/restricted records are included completely.

Credentials, email/authentication identity, passkeys, sessions, secrets, follows,
blocks, reports, operator grants, moderation restrictions/audits and import
provenance are outside this personal-diary format. No operational privileges or
moderation state can be applied by import. Unknown object fields are rejected,
including attempts to supply a destination owner or authentication material.
Unknown format versions are rejected, not guessed. Nullable archive fields must
be present explicitly (use `null` for unknown); missing fields are not silently
converted to unknown values.

### Restoration, privacy and ordering

All new content belongs to the authenticated importer. File-supplied source IDs
are relationship keys, never destination ownership or permission claims. New
random UUIDs replace profile/item identities, and notes are attached through the
owner-scoped playthrough mapping. Existing destination profile identity is kept;
handles are exported for reference but never claimed, renamed or reserved by
import. Return mappings describe every processed source record, including skips.

**Every newly imported component is private.** All file-supplied publication
flags and visibility selections are ignored, even when the destination profile
is already enabled. `restore` leaves the profile disabled and biography/statistics
private; `keep` leaves its existing choices untouched. Import does not restore
sharing or lift moderation restrictions. Publishing imported content requires
separate explicit, revision-protected publication operations afterwards.

Text, Unicode, whitespace, chosen calendar dates, unknown (`null`) dates, states,
ratings, list order, favorites and current-run relationships are preserved without
normalization. Current runs require an active/paused state, no finish date, and a
matching playing/on_hold library membership. No history is inferred from library
status. Source bookkeeping is never installed: destination creation/update times
and revisions describe the actual import. Consequently private date-added sorting
starts at import time. Public chronology still uses chosen dates only. New IDs
preserve relative source UUID ordering within an import batch, including across
notes and runs, so tied and undated records and derived-rating selection retain
their order in a fresh full round trip. Existing destination records and later
import batches participate using their own destination UUIDs; relative ties
between batches are not promised.

Game identity is the IGDB ID, not the archived name/slug. Existing stored games
are reused without provider access or metadata changes. For missing games, preview
reports sorted `missing_game_ids`; apply obtains trusted metadata from IGDB before
acquiring write locks. Archived names/slugs never modify the shared catalog. If a
game cannot be resolved (including a provider outage or retired identifier), the
whole import fails with the ordinary provider error plus the failing ID in
`error.fields`; fix that problem and retry. No partial diary or catalog is saved.

### Create-only merge and duplicates

Version 1 supports a **create-only merge**, with no destructive replace mode.
Existing library membership for an incoming new entry, an existing favorites
collection for incoming new favorites, or a competing current run is a conflict;
none is overwritten, cleared, merged implicitly or made private. Historical runs
and custom lists may be added alongside unrelated existing history. Use the
ordinary revision-protected APIs for intentional edits/reordering after resolving
conflicts. Preview is advisory, not an authorization token or reservation; apply
checks the current destination again under transaction locks.

The durable duplicate key is `(importing owner, source profile UUID, record kind,
source record UUID)`. A SHA-256 fingerprint covers that record's personal content
and relationships, including list order, but excludes bookkeeping, handle and
publication choices. An identical previously imported record is skipped, including
on simultaneous retries or after a lost response. Changing object/collection order
or ignored metadata does not make a new record. Changing content under an already
imported key produces `409 import_conflict`, with the record's JSON path in
`error.fields`. New source records can be added in a later archive; previously
imported parents can support new notes if the destination parent still exists.

Retries never undo destination edits, overwrite later publication decisions or
resurrect deleted content. Provenance keeps only the key, destination ID and hash,
not a second copy of private text. It remains until account deletion; target IDs
in skip mappings can therefore refer to deleted records. Deleting a record does
not erase its duplicate protection. Do not regenerate source identities to retry:
changing source IDs declares new content, and separately exported accounts are
separate sources. Equal review text alone is not a duplicate, since repeat
experiences can legitimately contain identical text. Ordinary creation retry
keys in feature 12 should coexist with this durable import provenance.

### Validation, reports and failure recovery

A preview report contains `valid`, `applied: false`, `create_count`, `skip_count`,
`missing_game_ids`, `errors` (field paths/codes/messages) and `mappings` for already
imported records. A successful apply has `applied: true`, an empty missing-game
list and mappings for both newly committed and skipped records. Counts describe
profile restoration as one record. `keep` omits that profile record. Fresh UUIDs
are not exposed or reserved by preview.

Malformed JSON returns `400 invalid_json`; wrong types, impossible date syntax
and unknown fields return `422 invalid_json`, using the standard error envelope
with field details where possible. Both endpoints reject trailing data after the JSON document. Preview
collects semantic errors into a `200` report with `valid: false`; apply returns
`400 validation_error` for invalid data and `409 import_conflict` for destination
or provenance conflicts. Clients can use the existing `ApiError.fields` mechanism.
All text/domain limits from the ordinary APIs apply, with no import-time trimming.
The graph must have distinct non-nil IDs per collection, one library row per game,
one current run per game, at most one favorites collection, valid parent references
within the archive, and at most 500 distinct games per ordered list.

Import requests have a dedicated 32 MiB body limit and at most 20,000 records
combined across games/library/playthroughs/notes/lists. Other routes retain their
64 KiB limit. Oversized bodies return 413; excess records are a validation error.
Export remains complete even above these import limits. Large archives can be
partitioned into smaller graph-complete batches: retain the same source profile
and record IDs, include each note's parent, and include exactly the games referenced
by that batch. Repeated parents/profile are skipped. Preserve whole lists; a list's
ordered membership is one record. Batch imports are independently atomic, so
preview each batch and resolve any earlier-import changes before continuing.

Each apply commits profile changes, catalog insertions, all personal records,
list membership and provenance in one database transaction. Per-owner import
locks serialize retries; existing per-game lifecycle locks and database constraints
arbitrate ordinary concurrent writes. A collision detected during insertion rolls
back everything and returns `import_conflict`; rerun preview. Provider errors,
injected database failures and cancellation before commit leave no partial import
or in-progress receipt. A lost response can safely be retried with the same archive.
Account deletion cascades to provenance as well as imported content. No background
job, external notifications, automatic sharing or authentication changes occur.
