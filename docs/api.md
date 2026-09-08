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
| POST | `/api/catalog/import/{igdb_id}` | Yes | Store one game or refresh metadata older than 24 hours |
| GET | `/api/me/library?status=playing&q=halo&limit=20&offset=0` | Yes | Own library with nested game details |
| GET | `/api/me/library/{igdb_id}` | Yes | Own entry for a game |
| PUT | `/api/me/library/{igdb_id}` | Yes | Create or replace own entry, storing the game if needed |
| DELETE | `/api/me/library/{igdb_id}` | Yes | Remove own library entry, preserving playthroughs (idempotent 204) |
| GET | `/api/me/playthroughs?igdb_id=42&limit=20&offset=0` | Yes | Own diary; optional game filter |
| POST | `/api/me/playthroughs` | Yes | Record a new experience, storing the game if needed (201) |
| GET | `/api/me/playthroughs/{id}` | Yes | Own playthrough |
| PUT | `/api/me/playthroughs/{id}` | Yes | Replace the editable fields of an existing playthrough |
| DELETE | `/api/me/playthroughs/{id}` | Yes | Delete own playthrough (idempotent 204) |

A stored game includes `id`, `igdb_id`, `name`, `slug`, nullable `summary`, `cover_url`, `first_release_date`, arrays of `{id,name}` for `platforms`, `genres`, `developers`, nullable `game_type`, `parent_igdb_id`, `version_parent_igdb_id`, `source_url`, `imported_at` and `refreshed_at`. Taxonomy/parent IDs are upstream IGDB IDs, not local UUIDs. The `game` nested in a library listing is that same object, field for field.

Catalog search results and catalog lookup share one descriptive shape: `igdb_id`, `name`, `slug`, nullable `summary`, `cover_url`, `first_release_date`, arrays of `{id,name}` for `platforms`, `genres`, `developers`, nullable `game_type`, `parent_igdb_id`, `version_parent_igdb_id`, and `source_url`. They carry no `id`, `imported_at` or `refreshed_at`, because a viewed game need not exist locally. Address them by `igdb_id`.

Use `GET /api/catalog/games/{igdb_id}` for frontend detail pages, including direct links and reloads. It returns a single catalog game object and needs no prior search, import, or sign-in. Search and lookup always query IGDB and never insert or refresh local games, create library entries, or record playthroughs. Imports and personal records still require authentication.

Catalog lookup accepts a positive signed 64-bit integer ID; malformed, overflowing, zero and negative IDs return `400 validation_error` before contacting IGDB. A game absent from IGDB returns `404 not_found`; provider failures retain the IGDB errors below, including `503 igdb_not_configured` when credentials are missing. Catalog lookup has no local fallback. Use `GET /api/games/{igdb_id}` when specifically reading stored metadata without IGDB; that route still returns 404 for games not stored here.

`PUT /api/me` body (both fields required):

```json
{"display_name":"Eddie","bio":"I like RPGs."}
```

Display name is trimmed and limited to 80 characters; bio to 1000. The response is the profile object with those fields plus `created_at` and `updated_at`. `GET /api/me` returns `{id,email,profile}`.

`PUT /api/me/library/{igdb_id}` body:

```json
{"status":"completed"}
```

Allowed statuses: `backlog`, `playing`, `completed`, `on_hold`, `dropped`. `status` is required and is the only writable library field. Each entry returns `igdb_id`, `status`, derived nullable `rating`, nullable `rating_playthrough_id`, `created_at`, and `updated_at`. Ratings and reviews cannot be written through this route; obsolete payloads containing either field are rejected rather than silently discarded. The library has no review field.

The displayed rating comes from the most recent **rated** playthrough, using the chronology defined below. `rating_playthrough_id` identifies that experience. An unrated replay does not hide a previous rating. Editing or deleting a playthrough recalculates the displayed rating automatically; if none remain rated, both derived fields are null. Changing library status never edits a playthrough. Library timestamps describe changes to library membership/status, not changes to the derived rating.

Saving an entry stores an unknown game automatically, so no import call is needed first. Existing metadata older than 24 hours currently triggers a refresh attempt; provider failure falls back to that local game. An unknown game requires a successful IGDB fetch, and an IGDB ID no game exists under returns 404.

## Playthroughs and diary

A playthrough represents one experience with a game, not an individual gaming session. Multiple entries for the same game are allowed. Completion is never required: abandoned runs and open-ended games can be rated or reviewed. Library membership and history are independent: recording a playthrough does not create a library entry or guess its status; removing a library entry preserves history. Deleting an account removes both.

`POST /api/me/playthroughs` body (`igdb_id` required, all other fields optional):

```json
{
  "igdb_id": 42,
  "rating": 9,
  "review": "A memorable first playthrough.",
  "started_on": "2022-03-01",
  "finished_on": "2022-04-15",
  "platform": "PC"
}
```

`PUT /api/me/playthroughs/{id}` accepts the same editable fields **without** `igdb_id`. The owner, game, UUID and recording time cannot be changed. PUT replaces all editable fields: omitted fields and explicit null both clear them. Editing preserves the playthrough's identity and `created_at`, and updates `updated_at`. To record a replay, POST a new entry. POST always creates a new UUID, so retrying a successful POST can create a duplicate.

- `rating`: null or an integer 1–10 (divide by two to show half-stars).
- `review`: null or plain text of at most 10000 characters, without NUL. Render user content as text, never as HTML.
- `started_on`, `finished_on`: null or calendar dates in `YYYY-MM-DD` format. If both are set, the finish must be on or after the start. These describe the experience; `finished_on` does not imply the game was completed or update library status. Either date can be provided alone.
- `platform`: null or a label of 1–100 characters without control characters, trimmed when saved. This is free text (for example `PC` or `Steam Deck`), not an IGDB taxonomy ID.

An undated rating, a review without a rating, and a record with neither are all valid. Unknown fields are rejected. Responses contain `id`, `igdb_id`, `rating`, `review`, `started_on`, `finished_on`, `platform`, `created_at`, and `updated_at`; they never expose the owner or local game key.

**Chronology:** newest `finished_on`, falling back to `started_on`, then the UTC date of `created_at`. Ties use `created_at` descending, then playthrough UUID descending. Both the diary and the derived library rating use this ordering; the latter skips unrated entries. Editing text never makes an old experience newest, while changing its dates can change its position. An undated entry uses its recording date until a play date is supplied, so an undated old experience cannot be distinguished from a recent one.

The migration converts each previous library rating and/or review into one undated playthrough, retaining the original recording/edit timestamps. It creates no playthrough for a status-only entry and invents no play dates or platform.

All library and playthrough routes use the authenticated session as owner. Reading or updating another user's playthrough returns 404; deletion is a no-op 204. History remains private, and this milestone has no public review/profile endpoints or review aggregation. Safe partial edits and stale-update protection remain follow-up work; playthrough PUT currently has replacement semantics with last-write-wins editing.

## Pagination

`limit` defaults to 20 and must be 1–50. `offset` defaults to zero and is capped at 10000. `q`, when supplied, must contain 1–100 characters without control characters. Local search matches case-insensitive name substrings, with `%` and `_` treated as ordinary characters rather than wildcards, over games stored here — those stored through library saves, playthrough creation, or explicit imports — while catalog search queries all of IGDB by relevance. The `status` filter applies to library listing.

Local games, library, and playthrough listings return:

```json
{"data":[],"limit":20,"offset":0,"has_more":false}
```

Local games sort by lowercase name then internal UUID. Library entries sort by most recently updated then internal game UUID, and each item is `{game,entry}`. Playthrough listing accepts only `limit`, `offset`, and optional positive `igdb_id`; it returns playthrough objects ordered by the chronology above. A game with no own playthroughs returns an empty page, even if it is not stored locally. Offset pagination can shift if entries change between requests.

Catalog search returns `{data,limit,offset,next_offset}`. A full upstream page sets the next offset (until the offset cap); it does not guarantee another result exists. No total-count query is made. Being read-only, it is safe to retry and cache. A successful import returns a single stored game and HTTP 200 whether newly created or refreshed.

## Errors

```json
{"error":{"code":"validation_error","message":"rating must be an integer from 1 to 10, or null"}}
```

Application error codes include `validation_error`, `invalid_json`, `not_found`, `method_not_allowed`, `request_rejected`, `internal_error`, `database_unavailable`, `igdb_not_configured`, `igdb_unavailable`, `igdb_rate_limited`, `catalog_busy`. Validation is generally 400; malformed JSON/schema/content type retains Axum's 400/422/415 status; oversized bodies return 413. Upstream failures return 502, unavailable/configuration/rate-limited upstreams return 503, and local catalog contention returns 429 with `Retry-After: 1`. Doorman auth errors retain Doorman's own codes, including `csrf_origin_mismatch` and `reauth_required`.

Every response carries an `x-request-id` for correlation. App/auth responses use `Cache-Control: no-store` to avoid leaking private or auth data through shared browser caches. No database details, upstream response bodies or credentials are returned in app errors.

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
```

The browser must run at `FRONTEND_ORIGIN`. Cookie writes require a matching Origin or Referer; the browser sends Origin automatically for these fetch requests. Cookie-based curl requests must include an equivalent header. Authenticated curl/tool requests can alternatively send `Authorization: Bearer <doorman-session-token>`; this is an opaque session token, not a JWT.

## Doorman routes and passkey flows

Mounted unchanged at `/auth`; authoritative DTOs and flow behavior come from the [pinned Doorman source](https://codeberg.org/EddieBrunstedt/Doorman/src/commit/7525cc3fdaf58e0905f6ba3677058319071e8380/crates/axum/src/dto.rs).

- Signup: `POST /auth/signup/request` with `{email}` → email link to SPA `/signup?token=...` → `POST /auth/signup/consume` with `{token}` → WebAuthn registration in browser → `POST /auth/signup/finish` with challenge ID and credential.
- Sign-in: `POST /auth/passkey/authenticate/start` → browser WebAuthn assertion → `POST /auth/passkey/authenticate/finish`.
- Recovery follows `/auth/recovery/request`, `/consume`, `/finish`, with an email link to the SPA `/recovery` screen and passkey enrollment.
- Passkeys: `GET /auth/passkey`, registration start/finish, rename and removal via `/auth/passkey/{credential_id}`.
- Sessions: `GET /auth/sessions`, `POST /auth/logout`, `POST /auth/logout-all`, and session revocation routes.
- Account: `POST /auth/account/email`, `POST /auth/account/email/confirm`, `DELETE /auth/account`.
- Re-authentication: `POST /auth/reauth/start`; provide the resulting fresh assertion in the protected action's `reauthentication` field according to Doorman's DTOs.

Email links only start signup/recovery; they do not create sessions on their own. Successful passkey finish sets the HttpOnly session cookie. Email changes, passkey removal and account deletion require re-authentication; recovery cooldowns also apply. Render frontend screens for `/signup`, `/recovery` and `/account/email`, consume their tokens once, and remove tokens from browser history after reading them. Use the [Doorman browser helper reference](https://codeberg.org/EddieBrunstedt/Doorman/src/commit/7525cc3fdaf58e0905f6ba3677058319071e8380/examples/web-client) to serialize WebAuthn ArrayBuffers correctly.
