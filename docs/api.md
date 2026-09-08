# API contract

Base URL: `http://localhost:3000`. All bodies are JSON unless a response is `204 No Content`. Timestamps use RFC 3339 UTC. Games are addressed by their IGDB integer ID everywhere, including in library routes; the local UUID in a game object is an internal primary key and never appears in a path. Request bodies are limited to 64 KiB.

## Application routes

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| GET | `/health/live` | No | Process liveness |
| GET | `/health/ready` | No | PostgreSQL readiness |
| GET | `/api/me` | Yes | Current user ID, email and app profile |
| PUT | `/api/me` | Yes | Replace display name and bio |
| GET | `/api/games?q=halo&limit=20&offset=0` | No | Search games stored locally |
| GET | `/api/games/{igdb_id}` | No | Locally stored game details |
| GET | `/api/catalog/search?q=halo&limit=20&offset=0` | Yes | Search IGDB; read-only, stores nothing |
| POST | `/api/catalog/import/{igdb_id}` | Yes | Store one game or refresh metadata older than 24 hours |
| GET | `/api/me/library?status=playing&q=halo&limit=20&offset=0` | Yes | Own library with nested game details |
| GET | `/api/me/library/{igdb_id}` | Yes | Own entry for a game |
| PUT | `/api/me/library/{igdb_id}` | Yes | Create or replace own entry, storing the game if needed |
| DELETE | `/api/me/library/{igdb_id}` | Yes | Remove own entry (idempotent 204) |

A stored game includes `id`, `igdb_id`, `name`, `slug`, nullable `summary`, `cover_url`, `first_release_date`, arrays of `{id,name}` for `platforms`, `genres`, `developers`, nullable `game_type`, `parent_igdb_id`, `version_parent_igdb_id`, `source_url`, `imported_at` and `refreshed_at`. Taxonomy/parent IDs are upstream IGDB IDs, not local UUIDs. The `game` nested in a library listing is that same object, field for field.

Catalog search results are a different shape: they carry the same descriptive fields but no `id`, `imported_at` or `refreshed_at`, because a searched game need not exist locally. Address them by `igdb_id`.

`PUT /api/me` body (both fields required):

```json
{"display_name":"Eddie","bio":"I like RPGs."}
```

Display name is trimmed and limited to 80 characters; bio to 1000. The response is the profile object with those fields plus `created_at` and `updated_at`. `GET /api/me` returns `{id,email,profile}`.

`PUT /api/me/library/{igdb_id}` body:

```json
{"status":"completed","rating":9,"review":"A memorable adventure."}
```

Allowed statuses: `backlog`, `playing`, `completed`, `on_hold`, `dropped`. `status` is required. `rating` is nullable or an integer 1–10 (divide by two to show half-stars). `review` is nullable and at most 10000 characters. PUT replaces the entry: omitting rating/review clears them, as does explicit null. Unknown fields are rejected. Each entry returns `igdb_id`, `status`, `rating`, `review`, `created_at`, `updated_at`. Use plain-text rendering for user content; never inject it as HTML.

Saving an entry stores the game automatically, so no import call is needed first: a game already held locally is reused, and an unknown one is fetched from IGDB in the same request. That fetch is the only reason a library write can fail with an upstream error (502/503) — a game already stored stays usable while IGDB is down, including with metadata past its refresh window. An IGDB ID no game exists under returns 404.

The authenticated session determines the owner; no route accepts a user ID for library writes. This milestone has no public review/profile endpoints or review aggregation.

## Pagination

`limit` defaults to 20 and must be 1–50. `offset` defaults to zero and is capped at 10000. `q`, when supplied, must contain 1–100 characters without control characters. Local search matches case-insensitive name substrings, with `%` and `_` treated as ordinary characters rather than wildcards, over games stored here — those at least one member has in a library, or that were explicitly imported — while catalog search queries all of IGDB by relevance. The `status` filter applies to library listing.

Local games and library listings return:

```json
{"data":[],"limit":20,"offset":0,"has_more":false}
```

Local games sort by lowercase name then internal UUID. Library entries sort by most recently updated then internal game UUID, and each item is `{game,entry}`. Offset pagination can shift if entries change between requests.

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
  body: JSON.stringify({ status: "playing", rating: null, review: null }),
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
