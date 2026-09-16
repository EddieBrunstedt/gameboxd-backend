# Gameboxd backend

Rust / Axum backend for a video game diary and library, ready for a future SolidJS SPA. Authentication is provided by [Doorman](https://codeberg.org/EddieBrunstedt/Doorman), pinned to commit `17bb0dcd509de6ca3bfd2742d3c9e422c92b4597`. Game metadata comes from [IGDB](https://api-docs.igdb.com/).

## Run locally

Requires Rust 1.94 or newer, PostgreSQL 17 with its contrib modules (the migrations create the `pg_trgm` extension, so the connecting role needs privileges to do so), and a C toolchain. WebAuthn pulls in OpenSSL through `webauthn-rs`, so OpenSSL development headers must be installed (`openssl` on Arch, `libssl-dev` on Debian/Ubuntu). Cargo.lock is included for reproducible dependency resolution.

```sh
cp .env.example .env
docker compose up -d --wait
cargo run --locked
```

Compose publishes PostgreSQL on `127.0.0.1:5433`, matching `DATABASE_URL` in `.env.example`. To use a database you manage yourself, skip `docker compose` and point `DATABASE_URL` at it. Stop the development database with `docker compose down`; add `-v` to discard its data.

The API listens on `http://localhost:3000`. Check `GET /health/live` and `GET /health/ready`. Migrations run on startup. The browser frontend will run at `http://localhost:5173`; use `localhost` consistently rather than mixing it with `127.0.0.1`.

## IGDB setup

Register a confidential application in the [Twitch developer console](https://dev.twitch.tv/console/apps), then set `IGDB_CLIENT_ID` and `IGDB_CLIENT_SECRET` in `.env`. Credentials stay on the backend. The application starts without them; operations requiring upstream metadata return `503 igdb_not_configured` (locally stored games still work).

Catalog search and detail lookup are public, read-only proxies: they query IGDB and return the same descriptive fields without writing anything. The SPA can use `GET /api/catalog/games/{igdb_id}` for direct links and page reloads without a prior search, import, or sign-in. A game earns a row in PostgreSQL when an authenticated user commits to it — adding it to a library or recording a playthrough — or through an explicit import, so the local catalog holds games this instance actually references. Games are named publicly by their IGDB ID; the local UUID is an internal primary key that never appears in a URL, which keeps the identifier a client needs available before any row exists.

Catalog lookup always contacts IGDB and reports provider errors without a local fallback. `GET /api/games/{igdb_id}` remains a separate read of stored metadata that works without IGDB and returns 404 for games not stored locally. Neither read inserts or refreshes a stored game. Public stored-game reads use the same descriptive shape as catalog lookup, omitting local UUIDs and import/refresh timestamps.

Importing a game reuses metadata refreshed within 24 hours and refreshes anything older in line, preserving the local UUID and every attached library entry and playthrough; if IGDB is unavailable, the stored game is served instead. Saving a library entry or a playthrough does not wait for that: an already-stored game is reused as it stands however old its metadata, and a stale one is queued for a background refresh, so a personal write is never delayed by provider latency, throttling or an outage. Only a game never seen before needs a successful upstream call, because there is otherwise no row for the record to reference. The refresh worker handles one game at a time and holds at most one pending refresh per game, so background work can never crowd out the imports a user is waiting on; a dropped or failed refresh only leaves metadata stale a while longer. Local game reads never require IGDB. The client caches OAuth tokens and refreshes them single-flight, retries once on an upstream 401, throttles requests below four per second, caps how many upstream requests are in flight at once and observes upstream rate limits. Callers past that cap queue for a slot rather than failing; `429 catalog_busy` is returned only when the queue behind it is also full. There is no full-catalog crawl or scheduled metadata synchronization yet.

IGDB requests visible attribution for commercial partnerships; arrange the partnership and display a persistent “Game data by IGDB” credit when building the frontend. See the [IGDB business FAQ](https://api-docs.igdb.com/#faq).

## Authentication and the SPA

Doorman supplies passkey signup, sign-in, recovery, passkey/session management, re-authentication and account deletion under `/auth`. Emails print to the terminal in development. Following a signup link requires the future SPA's `/signup` screen to complete WebAuthn; this repository has no frontend or fake-login endpoint.

Use browser `fetch` with `credentials: "include"`. CORS allows only `FRONTEND_ORIGIN`, and Doorman verifies that origin for cookie-authenticated writes throughout the app. Session cookies are HttpOnly, SameSite=Lax and Secure over HTTPS. Production uses the `__Host-gameboxd_session` cookie name. The API also accepts Doorman bearer sessions for non-browser clients; the SPA should use cookies and should not persist the token returned by Doorman in localStorage.

The first `/api/me` request creates the user's app profile. Authenticated users can update that profile and their own library. Personal content starts private; owners can explicitly publish library entries, playthrough summaries, ratings, reviews, and notes independently, behind a separate profile publication switch. One library entry per user/game stores the current status. Separate playthroughs preserve repeat experiences with optional ratings, reviews, dates and platform labels. An optional owner-chosen `presentation_on` day orders the diary and selects its derived library rating; unknown dates stay undated and unrated replays do not clear a rating. Play dates and private recording timestamps never supply a missing presentation day. See the historical chronology policy in [docs/api.md](docs/api.md#historical-chronology-policy). Playthroughs support both replacement (`PUT`) and partial (`PATCH`) edits, and both require an `If-Match` precondition, so a rating widget cannot erase a review and a stale tab cannot overwrite newer work. Each playthrough also supports multiple private dated diary notes, with independent revisions and paginated per-run/personal diary reads carrying local game context. Note edits and deletion require `If-Match`. History and notes are independent of library membership; playthrough deletion cascades to its notes. Public item routes use separate safe projections with live visibility checks and no-store responses. Public profiles have stable random UUIDs, optional renameable handles, visibility-filtered sections and authenticated visitor preview, including before enabling publication. Biographies remain private unless explicitly shared. See the [profile API policy](docs/api.md#public-profiles-stable-links-and-visitor-preview); social features follow later. Distinct IGDB editions retain distinct local records, with upstream parent IDs retained for future grouping.

See [docs/api.md](docs/api.md) for routes, request bodies, errors and the frontend contract. [contract/](contract/) publishes that contract as TypeScript: wire types generated from the Rust structs, a typed client for the application routes, and the browser adapter for Doorman's passkey flows.

Library and diary browsing supports game/date/year/state and rating/review filters, note text search, snapshot-consistent totals, and library status facets. Libraries sort by title, rating, membership date, or explicit recorded play dates; review edits never count as play. Dated collections offer cursor continuation across backfills. Public sections and owner preview filter only shared components and approved dates. See [browsing semantics](docs/api.md#library-and-diary-browsing).

## Verify

```sh
scripts/verify.sh --database
```

Or run the checks directly:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
# Requires TEST_DATABASE_URL in the environment or .env (see .env.example).
cargo test --locked --test backend -- --ignored
```

Database tests create randomly named disposable databases using a role with CREATE DATABASE privileges. They never apply migrations to the database named in TEST_DATABASE_URL. The database tests are explicitly ignored by default, and the separate command above and CI run them. Each successful test drops its database; a panicked test may leave a `gameboxd_test_*` database for inspection.

Provider tests use a local mock HTTP server, so tests do not require Twitch credentials. Database tests provision users/sessions through Doorman's Rust APIs and send HTTP requests through its real middleware. They cover migrations across restart, imports/refresh, background refresh of stale metadata after a save, saves that reuse a stored game while the provider is failing or slow, ownership, library edits, repeat playthroughs, atomic current-run lifecycle actions, concurrent start/transition/status/removal requests, rollback after injected database failures, historical state corrections, diary chronology, derived ratings, preservation of legacy reviews, validation, cookies/CSRF/CORS, re-authentication gates, logout and deletion cascades. Sharing tests cover independent permissions, visitor/owner/other-user access, publication races, revocation, public-only rating derivation, safe response bodies/headers, and private migration defaults. Note tests cover historical ordering, validation, ownership, independent revisions, concurrent edits/deletion, replay and membership retention, and account cascades. Playthrough editing is covered end to end: partial versus replacing semantics, mandatory and malformed preconditions, stale and weak entity tags, conditional deletion, and two simultaneous edits to one record. Authentication regressions also exercise a real socket, the 60/minute authentication-start limit, spoofed forwarding headers, independent IP buckets, missing connection metadata, and persisted audit IPs. Router tests supply `ConnectInfo<SocketAddr>` as a request extension to represent server connection metadata. Hardware/browser passkey ceremonies and live email delivery still need acceptance testing with the future frontend.

## Structure

- `src/config.rs`: validated environment configuration.
- `src/db.rs`, `migrations/`: database setup and app schema.
- `src/auth.rs`: Doorman assembly, SMTP and audit persistence.
- `src/igdb.rs`, `src/models.rs`: provider client and stable local game records.
- `src/refresh.rs`: background metadata refresh, so saves do not wait on the provider.
- `src/api.rs`: game, owner profile, library and playthrough endpoints.
- `src/api/browsing.rs`: validated collection filters, explicit sort policies, snapshot totals/facets, and historical cursor paging.
- `src/api/profiles.rs`: public identity, profile sections and authenticated visitor preview.
- `src/api/sharing.rs`: item publication controls and dedicated public projections.
- `src/lib.rs`, `src/main.rs`: routing, middleware, startup and shutdown.
- `tests/`: provider tests and PostgreSQL integration tests.

## Deployment boundary

This is a backend foundation for one application process. Before deployment, use HTTPS, configure SMTP, and put the SPA and API on the same origin via a reverse proxy (recommended) or compatible same-site hosts. Arbitrary cross-site SPA/API hosting is not supported by the SameSite=Lax cookie configuration. Set `APP_ENV=production`, the exact browser `FRONTEND_ORIGIN` and correct WebAuthn `RP_ID`; changing the RP ID later can invalidate existing passkeys.

Production SMTP uses authenticated STARTTLS. Audit records persist in PostgreSQL; Doorman's audit sink is best-effort, with failures logged. Set retention and backups for these potentially sensitive records. Expired authentication sessions and challenges are purged every five minutes. Request logging excludes query strings, headers and bodies; do not enable development terminal email in production (configuration rejects it).

Authentication rate limiting and IGDB throttling are per process. Before running multiple replicas, add shared limiting and review Doorman's session cache invalidation across replicas.

Public catalog search and lookup share the bounded IGDB queue and throttle with authenticated imports. Before public launch, add per-client catalog limits and edge abuse controls so one visitor cannot consume the shared provider capacity.

Authentication client identity comes from the immediate socket peer via Axum's `ConnectInfo<SocketAddr>`, supplied by the server in `src/main.rs`. Doorman uses that IP for rate-limit buckets and recorded client IPs, ignoring the source port and incoming `Forwarded`, `X-Forwarded-For`, and `X-Real-IP` headers. Authentication routes that require client identity reject missing connection metadata with `500 internal_error`.

This backend has no trusted-proxy IP resolver. Behind a reverse proxy, all clients reaching the backend through the same proxy IP share its authentication limits and are recorded under that IP. Before relying on per-client limits behind a proxy, implement host middleware that validates the immediate peer against explicitly trusted proxies, resolves and validates the forwarding chain, and inserts Doorman's typed `ClientIp` request extension before authentication handlers run. Configure that policy and edge abuse controls for the actual hosting topology.

Authentication tables and their migration history live in `gameboxd_auth`; application tables and migration history live in `gameboxd`. `pg_trgm` is installed into `gameboxd` on a fresh database, or reused wherever it already exists. The only schema coupling is a foreign key to Doorman's BYTEA user ID, with deletion cascades for profiles, library entries and playthroughs (and their notes). Check this against any Doorman revision update. Runtime application queries never directly mutate authentication tables.
