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
  client.ts                  typed client for /api routes
  auth.ts                    browser adapter for /auth routes
  doorman/passkey.upstream.ts  pristine copy of Doorman's reference client
  index.ts                   what a consumer imports
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
appears in `docs/api.md`, and the two Doorman checks below.

## Errors

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

Gameboxd changes three things, all of which the diff shows:

- **A base URL.** The SPA and the API are separate origins; the upstream
  reference assumes it is served from the backend's origin.
- **Re-authentication.** Upstream sends destructive requests without proof and
  hands the resulting 401 to the caller. `withReauth` runs the step-up ceremony
  and retries once, so removing a passkey, changing an email or deleting an
  account works without the UI reimplementing it three times.
- **`isSignedOut` / `isReauthRequired`.** Both are 401. Redirecting to sign-in
  on every 401 would throw away whatever the user was in the middle of doing at
  the exact moment the server asked them to confirm it was them. **Never branch
  on the status alone.**

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
