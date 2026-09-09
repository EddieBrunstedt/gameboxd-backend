// The error vocabulary shared by both halves of the contract.
//
// This file exists because a 401 does not belong to one of them. Doorman's
// session layer sits in front of *every* route (see `router` in `src/lib.rs`),
// so the application routes in `client.ts` reject with Doorman's codes, not
// with the application's own. `GET /api/me` without a session answers
// `401 session_not_found` — a code that `ErrorCode` deliberately does not
// contain, because `ErrorCode` is generated from the codes the *application*
// emits.
//
// Keeping the two vocabularies in separate files was the original mistake: it
// left `ApiError.code` typed as a union that could not name the codes it
// actually carries, so `error.code === "session_not_found"` was a comparison
// TypeScript rejected as impossible. The sign-out branch every screen needs was
// unwritable in the type system. Both clients now throw errors that extend one
// base, and the predicates below work on either.

import type { ErrorCode } from "./types/ErrorCode";

/**
 * Every code Doorman can emit, from `classify` in its `crates/axum/src/error.rs`
 * plus `csrf_origin_mismatch` from its CSRF layer.
 *
 * Hand-written and drift-checked: `tests/contract.rs` parses `classify` at the
 * pinned revision and fails if this union and that match disagree in either
 * direction. Doorman is a git dependency, so a moved pin can add or rename a
 * code, and a union that quietly lagged behind would be worse than none.
 */
export type AuthCode =
  | "account_disabled"
  | "csrf_origin_mismatch"
  | "email_already_in_use"
  | "email_error"
  | "internal_error"
  | "invalid_email"
  | "invalid_operator_actor"
  | "invalid_passkey_challenge"
  | "invalid_relying_party"
  | "invalid_verification_token"
  | "passkey_not_found"
  | "passkey_ownership_mismatch"
  | "passkey_possible_compromise"
  | "rate_limited"
  | "reauth_required"
  | "recovery_cooldown_active"
  | "resource_limit_exceeded"
  | "session_expired"
  | "session_not_found"
  | "signup_not_allowed"
  | "storage_error"
  | "user_not_found"
  | "user_verification_required"
  | "webauthn_error";

/**
 * Codes raised in the browser, never received from the server.
 *
 * `navigator.credentials.create()` and `.get()` reject with a `DOMException`;
 * those rejections are the ordinary outcomes of a passkey prompt — the user
 * dismisses it, it times out, the authenticator already holds a credential —
 * and a UI has to tell them apart to know whether to say anything at all.
 * Giving them codes puts them in the same `catch` as everything else instead of
 * leaving a bare `DOMException` to escape into a generic error toast.
 */
export type ClientCode =
  | "passkey_cancelled"
  | "passkey_already_registered"
  | "passkey_unsupported"
  | "passkey_origin_mismatch";

/** Anything `code` can be on an error thrown by this contract. */
export type AnyErrorCode = ErrorCode | AuthCode | ClientCode;

/**
 * The base both clients throw.
 *
 * `status` and `code` are the two things every call site branches on, so they
 * live here rather than being declared twice with the risk of drifting apart.
 * `ApiError` adds `fields`; `AuthError` adds `guidance`.
 */
export class GameboxdError extends Error {
  constructor(
    readonly status: number,
    readonly code: AnyErrorCode | null,
    message: string,
  ) {
    super(message);
    this.name = "GameboxdError";
  }
}

/** The code carried by any error this contract threw, or null for anything else. */
export function codeOf(error: unknown): AnyErrorCode | null {
  return error instanceof GameboxdError ? error.code : null;
}

/**
 * True when the session is gone and signing in again is the only way forward.
 *
 * Deliberately not "the response was 401": `reauth_required` is also a 401, and
 * treating it as a sign-out would throw away whatever the user was in the
 * middle of doing at the exact moment the server asked them to confirm it was
 * them. **Never branch on the status alone.**
 *
 * `session_expired` comes only from `/auth` routes that consult the session
 * manager directly. An expired cookie on an application route reads as
 * `session_not_found`, because Doorman's session layer attaches no context for
 * a session it could not validate and the `AuthUser` extractor cannot tell
 * "expired" from "never existed". Both are listed because a caller should not
 * have to know which route it was talking to.
 */
export function isSignedOut(error: unknown): boolean {
  const code = codeOf(error);
  return code === "session_not_found" || code === "session_expired";
}

/** True when the session is valid but the action needs a fresh assertion. */
export function isReauthRequired(error: unknown): boolean {
  return codeOf(error) === "reauth_required";
}

/**
 * True when the account is disabled. Separate from {@link isSignedOut} on
 * purpose: it is a 403, and signing in again does not clear it, so sending the
 * user to the sign-in screen would be an invitation to a loop.
 */
export function isAccountDisabled(error: unknown): boolean {
  return codeOf(error) === "account_disabled";
}

/**
 * True when the user dismissed a passkey prompt or it timed out.
 *
 * Worth its own predicate because it is the one failure that should usually
 * produce no message at all: the user cancelled, they know they cancelled, and
 * an error banner telling them so is noise. WebAuthn reports a dismissal and a
 * timeout identically, so these cannot be distinguished.
 */
export function isPasskeyCancelled(error: unknown): boolean {
  return codeOf(error) === "passkey_cancelled";
}

/**
 * True when the cool-down after account recovery is blocking an action.
 *
 * Recovering with a new passkey starts a 24-hour window in which destructive
 * actions stay blocked, so a stolen email cannot immediately be turned into a
 * takeover. The UI has to explain this rather than report a generic failure.
 */
export function isRecoveryCooldown(error: unknown): boolean {
  return codeOf(error) === "recovery_cooldown_active";
}

// Jargon -> end-user guidance. This is the table a host customises per product
// (wording, localisation). The server keys are the codes doorman-axum emits;
// the rest are the browser-side ceremony outcomes above.
export const ERROR_GUIDANCE: Record<string, string> = {
  recovery_cooldown_active:
    "This action is blocked during the 24-hour recovery cool-down. You can still add a passkey and sign out of this session.",
  csrf_origin_mismatch:
    "The request was blocked by the cross-origin check. Make sure you are using the app's real address.",
  session_not_found: "You are signed out. Sign in with a passkey to continue.",
  session_expired: "Your session expired. Sign in with a passkey again.",
  email_already_in_use: "That email is already linked to an account.",
  invalid_email: "That does not look like a valid email address.",
  invalid_verification_token: "This link is invalid or has already been used. Request a new one.",
  invalid_passkey_challenge: "The passkey step timed out. Try again.",
  passkey_not_found: "That passkey is no longer registered.",
  passkey_ownership_mismatch: "That passkey does not belong to this account.",
  passkey_possible_compromise:
    "Sign-in was blocked because this authenticator may have been cloned. Use another passkey and review your account.",
  rate_limited: "Too many attempts. Wait a little while and try again.",
  resource_limit_exceeded: "You have reached the limit for this resource.",
  webauthn_error: "The passkey step failed. Try again, or use a different authenticator.",
  user_not_found: "No matching account.",
  // Upstream's table omits this one. It means the authenticator completed the
  // ceremony without verifying the person holding it — no PIN, no biometric —
  // while the server required that it did.
  user_verification_required:
    "Your device did not confirm it was you. Use a passkey protected by a PIN, fingerprint or face unlock.",
  account_disabled: "This account has been disabled. Contact support.",
  // Present so nothing falls through to the signed-out wording below: this 401
  // means the session is still good and the action needs fresh proof.
  reauth_required: "Confirm it is you with your passkey to continue.",

  // Browser-side ceremony outcomes.
  passkey_cancelled: "The passkey prompt was dismissed or timed out.",
  passkey_already_registered:
    "This device already has a passkey for this account. Sign in with it instead of adding another.",
  passkey_unsupported: "This browser or device cannot use passkeys.",
  passkey_origin_mismatch:
    "Passkeys are not available on this address. Open the app at its configured origin.",
};

export function guidanceFor(code: string | null, status: number): string {
  if (code && ERROR_GUIDANCE[code]) return ERROR_GUIDANCE[code];
  if (status === 401) return ERROR_GUIDANCE.session_not_found;
  return "Something went wrong. Try again.";
}
