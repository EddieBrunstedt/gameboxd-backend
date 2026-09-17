# Frontend contract

Everything the SPA needs to talk to this backend: the wire types, a typed
client for the application routes, and the browser adapter for Doorman's
passkey flows.

There is no `package.json` here and no build step. These are plain `.ts` files
that import nothing but each other, so the frontend — which does not exist yet
— can consume them by relative import, by a copy script, or as a workspace
package, whichever it turns out to want.

```
contract/
  types/                     generated from the Rust structs; never edit
  errors.ts                  the code vocabulary and predicates both clients share
  client.ts                  typed client for /api routes
  auth.ts                    browser adapter for /auth routes
  doorman/passkey.upstream.ts  pristine copy of Doorman's reference client
  index.ts                   what a consumer imports
  tsconfig.json              type-check settings; no build, `noEmit` is set
```

## What is generated, and what is not

`contract/types` is produced by [ts-rs](https://github.com/Aleph-Alpha/ts-rs)
from the same structs that serialize the responses. There is no second
declaration of any field anywhere: rename a field in Rust and the TypeScript
renames, add a column to `Game` and the type grows. Regenerate with:

```sh
cargo test --lib export_bindings
```

CI runs the full test suite and then fails on a dirty working tree, so a struct
changed without regenerating is a failed build rather than a silent lie.

Two settings in `.cargo/config.toml` matter. `TS_RS_EXPORT_DIR` puts the output
here, and `TS_RS_LARGE_INT = "number"` overrides ts-rs's default of `bigint` for
`i64` — a `bigint` field would typecheck against nothing `JSON.parse` returns,
and every 64-bit value this API publishes (IGDB IDs, taxonomy IDs) is far below
2^53.

The route table in `client.ts` is hand-written and has to be. Axum's router
erases paths, methods and status codes at the type level, so no generator can
recover them. It changes rarely, and a mistake fails immediately as a 404 or a
405 — unlike a wrong field type, which fails quietly at the point of use. That
split is the whole design: generate what is easy to get subtly wrong, write by
hand what fails loudly.

`tests/contract.rs` checks the parts that are hand-written: that `client.ts`
only imports types that were actually generated, that every `ErrorCode` variant
appears in `docs/api.md`, that every published code a user can act on has
guidance copy, and the two Doorman checks below.

## Errors

`errors.ts` holds the vocabulary, because errors are the one thing that does not
split cleanly along the `/api` and `/auth` line. Doorman's session layer fronts
the whole router, so an application route rejects with *Doorman's* codes when
there is no session — `GET /api/me` without a cookie is `401 session_not_found`.

That is what the readiness review found: `ApiError.code` was typed as the
generated `ErrorCode` union, which deliberately excludes Doorman's codes, so
`error.code === "session_not_found"` was a comparison TypeScript rejected as
impossible. The sign-out branch every screen needs could not be written. Both
clients now throw errors extending one `GameboxdError` base, `code` is typed
`AnyErrorCode` — the application's codes, Doorman's, and the browser-side
ceremony codes — and the predicates work on either class:

- `isSignedOut` — the session is gone; sign-in is the only way forward. Accepts
  `session_not_found` **and** `session_expired`, because an expired cookie on an
  application route reads as the former: the session layer attaches no context
  for a session it could not validate, and the extractor cannot tell "expired"
  from "never existed".
- `isReauthRequired` — the session is fine and the action needs fresh proof.
  Also a 401. **Never branch on the status alone.**
- `isPasskeyCancelled` — the user dismissed the prompt. Usually deserves no
  message at all.
- `isAccountDisabled`, `isRecoveryCooldown`, and `isStale` in `client.ts`.

`AuthCode` is hand-written and drift-checked against Doorman's own `classify` at
the pinned revision by `tests/contract.rs`, in both directions, so a moved pin
that adds or renames a code fails the build rather than leaving a union quietly
behind. `ERROR_GUIDANCE` maps every code a user can act on to product copy; a
missing entry is a test failure too.

`ApiError` in `client.ts` carries the `code` union, the message, and `fields`.

`fields` is present exactly when individual fields were rejected, and a form
should branch on that rather than on the code — the same array arrives whether
our own validation rejected a value (`400 validation_error`) or serde rejected
the shape before the handler ran (`422 invalid_json`). `error.byField()` keys
them by name for rendering beside a control.

Every failing field of one submission comes back together, so a form marks
everything it got wrong in a single round trip.

## Authentication

`auth.ts` is derived by hand from Doorman's reference browser client at the
revision this backend pins,
`17bb0dcd509de6ca3bfd2742d3c9e422c92b4597`. The unmodified original sits in
`doorman/passkey.upstream.ts` and is byte-compared against the pinned checkout
by `tests/contract.rs`. When the pin moves and that file changed, the test
fails: copy the new upstream over it and re-derive the adapter from the diff.
Nothing about the auth surface is inherited silently.

Gameboxd changes four things, all of which the diff shows:

- **A base URL.** The SPA and the API are separate origins; the upstream
  reference assumes it is served from the backend's origin.
- **Re-authentication.** Upstream sends destructive requests without proof and
  hands the resulting 401 to the caller. `withReauth` runs the step-up ceremony
  and retries once, so removing a passkey, changing an email or deleting an
  account works without the UI reimplementing it three times.
- **The error vocabulary**, moved to `errors.ts` and shared with `client.ts`.
  Upstream's predicates only understood upstream's error class, which is wrong
  for every screen that calls an `/api` route.
- **Ceremony failures.** `navigator.credentials.create()` and `.get()` *reject*
  with a `DOMException` rather than resolving `null`, so upstream's `if (!cred)`
  guard almost never fires and a dismissed prompt — the most common outcome of
  all — escapes as an unrecognised exception carrying browser-authored text.
  They are translated into `AuthError`s with the `ClientCode` codes instead.

## Type-checking

There is still no build step and no `package.json`. `tsconfig.json` sets
`noEmit` and exists so the files can be proven strict-clean:

```sh
tsc -p contract     # or: npx -y -p typescript@7.0.2 tsc -p contract
```

CI runs this. It has to: the bug above was a type error and nothing in the Rust
suite could see it.

## Using it

```ts
import { configureGameboxd, me, updatePlaythrough, isStale, ApiError } from "./contract";

configureGameboxd({ baseUrl: "http://localhost:3000" });

const user = await me();

try {
  const saved = await updatePlaythrough(current, { review: text });
} catch (error) {
  if (isStale(error)) {
    // Someone else saved first: reload the playthrough and reapply the edit.
  } else if (error instanceof ApiError) {
    const problems = error.byField();
  }
}
```

Playthrough edits take the record, not an id, because every edit must declare
the revision it was based on and `version` travels in the body — including
inside listings, so an editor opened from a list needs no extra read.

`presentation_on` is a nullable calendar-date string for the chosen historical
day, independent of play dates and recording timestamps. Create/PUT omission
means unknown; PATCH omission preserves it and null clears it. Preserve the
string as a day, without time-zone conversion. Diary and rating selection use
this day descending, unknown dates last, then UUID descending. See the
[chronology and future public contract](../docs/api.md#historical-chronology-policy).
These generated playthrough types and revision tags are for owner routes only.

`startPlaythrough` creates a current run/replay and sets library status in one
transaction. `transitionPlaythrough(current, input)` carries the revision for
pause/resume/complete/abandon. `state` is nullable (unknown); `is_current` is
read-only. `createPlaythrough` and historical PUT/PATCH never change present
status. Library entries identify the current run with `current_playthrough_id`.
Conflicting status-only writes or invalid lifecycle actions return
`409 lifecycle_conflict`. Library removal detaches the current run without
removing history. See [lifecycle semantics](../docs/api.md#current-playthrough-lifecycle).

`diary()` lists the owner's notes across all playthroughs; `playthroughNotes(id)`
limits them to one run. Both return paginated `DiaryNote` values with local game
context. `createNote(id, input)` attaches a private note to that run.
`replaceNote`, `updateNote`, and `deleteNote` require the current note object and
send its independent revision through `If-Match`; `isStale` handles conflicts.
Text is required and nonblank. PUT requires text and clears an omitted date;
PATCH preserves omitted fields and permits null only for `presentation_on`.
Dates and ordering follow the same historical policy as playthroughs. Removing
library membership or replaying preserves notes; deleting a playthrough deletes
all its notes. These types contain owner-only bookkeeping and are not public
sharing representations. See [note semantics](../docs/api.md#dated-diary-notes).


`setPublication(profile, input)` controls the profile gate. `setLibraryVisibility`,
`setPlaythroughVisibility`, and `setNoteVisibility` take the current owner resource
and send its revision. Ordinary create/edit inputs do not set permissions. Runs
have independent `summary_visibility`, `rating_visibility`, and `review_visibility`;
notes/library entries have `visibility`. All default private. Clearing a rating or
review clears its publication choice. Profile disabling preserves item choices.

`publicLibraryEntry`, `publicPlaythrough`, `publicRating`, `publicReview`, and
`publicNote` return dedicated `Public*` types. Library public reads take the entry's
`public_id`; other reads take the run/note UUID. These responses contain no owner
bookkeeping or implicit parent details. The public library rating uses only public
ratings. Public routes give owners the same result as anonymous visitors and return
404 when unavailable. Refetch public views: all responses are `no-store`, with no
public revision validators. Public profiles and visitor preview reuse those projections.
Public local `games`/`game` now return `CatalogGame` descriptions, while authenticated
imports and owner library game objects retain `Game` bookkeeping. See the
[sharing contract](../docs/api.md#per-item-visibility-and-public-representations).


`setHandle(profile, {handle})` carries the owner profile revision and returns the
updated `Profile`. Its `public_id` is the canonical stable profile UUID; handle
renames cannot change it. `publicProfile(id)` and `publicProfileByHandle(handle)`
return `PublicProfile` with selected counts. Retired handles return 404 and cannot
be taken by another account. `handle_unavailable` means choose another handle.

`publicProfileSection(id, section, {limit,offset})` returns the generated
`PublicProfileSection` discriminated union; narrow on `section` to obtain the
corresponding `page.data` type. `publicPreview()` and `publicPreviewSection()`
return exactly the same public types for the session owner, including while their
profile is disabled. Private components stay hidden. Read the owner's `Profile`
to label publication state; it is not part of the preview response.

Biographies default private. `setPublication` accepts optional `bio_visibility`;
omission/null preserves it and publishing the profile alone never publishes a
bio. Each public item has an `author_id` linking to the stable public profile.
See the [profile, handle and preview contract](../docs/api.md#public-profiles-stable-links-and-visitor-preview).

## Library and diary browsing

`library`, `playthroughs`, `diary`, `playthroughNotes`, public profile sections,
and preview sections use generated `BrowsePage<T>` responses: data, bounded
limit/offset, `has_more`, filtered `total`, `next_cursor`, and nullable
`status_counts`. The owner library row adds nullable `last_played_on`. Local
game/catalog pagination is unchanged.

Use the client's `LibraryQuery`, `PlaythroughQuery`, and `NoteQuery` types for
owner reads; generated `BrowseQuery` describes the full query vocabulary. Public
section capabilities are validated server-side. Unsupported combinations return
400, including public membership-date sorting or filtering public notes by
parent state. Boolean filters serialize as `true`/`false`.

Dated collections can continue with `{...filters, after: page.next_cursor}`
(omit offset), retaining the same filters and collection. Refresh after changing
filters or chosen dates. Totals cover the entire matching set, and library status
facets ignore only the selected status. Review edits never affect recent-play
sorting. See [the complete browsing policy](../docs/api.md#library-and-diary-browsing)
for null/tie orders, date semantics, public-only inputs, and concurrent changes.


## Lists and favorites

`lists`, `list`, `createList`, `replaceList`, `setListGames`, `setListVisibility`,
and `deleteList` manage ordered collections. Creation always starts empty/private;
`kind: "favorites"` creates the owner's single favorites list (`favorites_exists`
on conflict). Other collections use `custom`. Detail results contain `{list,games}`;
pass the nested `list` to edit/delete functions so they send its current revision.

`setListGames` replaces the entire ordered membership (up to 500 distinct positive
IGDB IDs, already imported). It supports addition, removal and reorder without
changing library or history. Edits to a public list explicitly publish the new
membership. On 412, reread and reconcile; never retry a stale array unconditionally.
The server rejects wildcard list preconditions. `backlog` means want-to-play, never
ownership; there is no competing built-in wishlist collection.

`publicList`, `publicLists`, profile section `lists`, and `previewList` expose
safe list projections. `ListQuery` supports only title `q`, `limit` and `offset`.
Profile counts include `lists`; favorites are identified by kind within that section.
Public/preview detail contains full ordered game context without personal status,
ratings, history, revisions or timestamps. See the
[complete contract](../docs/api.md#favorites-ordered-lists-and-want-to-play).

## Statistics

`statistics(query)` reads personal aggregates. `publicStatistics(profileId, query)`
and `previewStatistics(query)` read only selected public components. All return
`Statistics`: recorded-run metrics, ten rating buckets, and current library status
counts. `StatisticsQuery` accepts `year` or inclusive `from_on`/`to_on`, plus
`undated`. Filters use chosen presentation dates; current status counts always
remain current. Replays count additional runs per game within the selected period;
ratings count entries, independently of summary publication.

Use `setPublication(profile, {public_enabled, statistics_visibility: "public"})`
to opt into a summary. This choice defaults private and omission/null preserves it.
Preview bypasses only the profile gate, so first select the public summary while
the profile is disabled to preview it before enabling. Private inputs never affect
public statistics. See [metric definitions and privacy](../docs/api.md#personal-statistics-and-explicitly-shared-summaries).

Private social reads use `following(query)` and `feed(query)`; writes use
idempotent `follow(publicProfileId)` / `unfollow(publicProfileId)` and return
204. The generated `SocialPage<T>` has `limit`, `has_more`, and `next_cursor`;
pass the cursor back as `after`. Following shows only currently public targets,
ordered by stable public UUID. `FeedItem` discriminates `review`, `note`, and
`milestone` payloads, identical to their direct public types. Key accumulated
items by `(kind, item.id)` and use only `item.presentation_on` for chronology.
Milestones represent one current chosen summary per run, not lifecycle audit
events. All reads are live/no-store; refresh after date changes or changes to
the followed set. See [the social API policy](../docs/api.md#private-following-and-chronological-gaming-life-feed)
for hidden targets, null dates, cursor semantics and revocation.

Safety client functions include `block`, `unblock`, `blocks`, `report`, and
`ownRestrictions`, plus operator-only report inspection/resolution, restrictions
and audit functions. Types are generated from the backend. Operator decisions
carry `expected_version` (0 for a new restriction, otherwise the fetched version)
and a nonblank reason. A 412 requires rereading before reconciliation. Private
evidence is untrusted `unknown | null`; inspect its target kind and render text
safely. Operator functions always require a live database-provisioned grant.
See [blocking and moderation semantics](../docs/api.md#blocking-reporting-and-operator-moderation).

Personal transfer uses generated `PersonalArchive`, `Archive*`,
`PersonalImportInput` and `PersonalImportReport` types. `exportPersonalData`,
`previewPersonalImport` and `importPersonalData` expose the authenticated routes.
Pass an explicit `profile_policy` (`restore` into a blank private profile, or
`keep` to retain the destination profile). Preview reports all semantic conflicts;
apply revalidates, imports privately and returns source-to-destination mappings.
`import_conflict` errors use the normal `ApiError.fields` contract. Identical
retries skip records, including after local edits/deletion. See the backend's
[format, limits and recovery policy](../docs/api.md#personal-export-and-round-trip-import).
