// Gameboxd's browser adapter for Doorman's passkey HTTP surface.
//
// Derived from `examples/web-client/passkey.ts` in Doorman at the revision this
// backend pins. The unmodified original sits beside this file as
// `doorman/passkey.upstream.ts` and is byte-compared against the pinned
// checkout by `tests/contract.rs`, so bumping the pin fails loudly here rather
// than letting this adapter drift away from the server it talks to. Diff the
// two files to see exactly what Gameboxd changed:
//
//   1. `configureAuth` — the SPA and the API are different origins, so every
//      request needs an absolute base URL. The upstream reference assumes it
//      is served from the same origin as the backend.
//   2. Re-authentication — the upstream reference sends destructive requests
//      without proof and surfaces the resulting 401 to the caller. Gameboxd
//      runs the step-up ceremony and retries, because a 401 meaning "prove it
//      again" must not be handled like a 401 meaning "you are signed out".
//   3. Error vocabulary — the codes, the guidance table and the predicates moved
//      to `./errors.ts`, shared with `./client.ts`. Doorman's session layer
//      fronts the application routes too, so its codes arrive there as well and
//      a predicate that only understood this file's error class was wrong for
//      every screen that called an `/api` route.
//   4. Ceremony failures — `navigator.credentials` rejects with a
//      `DOMException`, which upstream lets escape raw. Those rejections are
//      translated into `AuthError`s carrying a code, because a dismissed prompt
//      is an ordinary outcome the UI has to recognise, not an unknown crash.
//
// The server serializes WebAuthn options and responses as base64url JSON.
// The browser APIs require ArrayBuffer fields, so these helpers translate
// challenge/user/credential IDs in both directions and then call the doorman
// endpoints for signup, sign-in, recovery, passkey management, and sessions.

import {
  type AnyErrorCode,
  type ClientCode,
  GameboxdError,
  guidanceFor,
  isReauthRequired,
} from "./errors";

// Re-exported so `auth.ts` stays the single import for authentication work,
// even though the vocabulary is now shared with the application client.
export {
  ERROR_GUIDANCE,
  guidanceFor,
  isAccountDisabled,
  isPasskeyCancelled,
  isReauthRequired,
  isRecoveryCooldown,
  isSignedOut,
} from "./errors";
export type { AnyErrorCode, AuthCode, ClientCode } from "./errors";

function base64UrlToBuffer(input: string): ArrayBuffer {
  const padded = input.padEnd(input.length + ((4 - (input.length % 4)) % 4), "=");
  const b64 = padded.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function bufferToBase64Url(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

type ServerCreationOptions = {
  publicKey: {
    challenge: string;
    user: { id: string; name: string; displayName: string };
    rp: { id: string; name: string };
    pubKeyCredParams: Array<{ type: "public-key"; alg: number }>;
    timeout?: number;
    attestation?: AttestationConveyancePreference;
    excludeCredentials?: Array<{ type: "public-key"; id: string; transports?: AuthenticatorTransport[] }>;
    authenticatorSelection?: AuthenticatorSelectionCriteria;
    extensions?: AuthenticationExtensionsClientInputs;
  };
};

type ServerRequestOptions = {
  publicKey: {
    challenge: string;
    timeout?: number;
    rpId?: string;
    allowCredentials?: Array<{ type: "public-key"; id: string; transports?: AuthenticatorTransport[] }>;
    userVerification?: UserVerificationRequirement;
    extensions?: AuthenticationExtensionsClientInputs;
  };
};

type Challenge<T> = { challenge_id: string; options: T };

export type UserDto = {
  id: string;
  email: string | null;
  email_verified: boolean;
  created_at: string;
  passkey_count: number;
};

export type SessionDto = {
  id: string;
  created_at: string;
  expires_at: string;
  ip: string | null;
  user_agent: string | null;
  device_label: string | null;
  is_current: boolean;
};

export type AuthResponse = {
  user: UserDto;
  session: SessionDto;
  token: string;
};

export type PasskeyDto = {
  credential_id: string;
  name: string | null;
  created_at: string;
  last_used_at: string | null;
  aaguid: string | null;
  transports: string[];
  backup_eligible: boolean;
  backup_state: boolean;
};

function decodeCreationOptions(o: ServerCreationOptions): CredentialCreationOptions {
  const p = o.publicKey;
  return {
    publicKey: {
      ...p,
      challenge: base64UrlToBuffer(p.challenge),
      user: { ...p.user, id: base64UrlToBuffer(p.user.id) },
      excludeCredentials: p.excludeCredentials?.map((c) => ({
        ...c,
        id: base64UrlToBuffer(c.id),
      })),
    },
  };
}

function decodeRequestOptions(o: ServerRequestOptions): CredentialRequestOptions {
  const p = o.publicKey;
  return {
    publicKey: {
      ...p,
      challenge: base64UrlToBuffer(p.challenge),
      allowCredentials: p.allowCredentials?.map((c) => ({
        ...c,
        id: base64UrlToBuffer(c.id),
      })),
    },
  };
}

function encodeAttestation(cred: PublicKeyCredential) {
  const att = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: bufferToBase64Url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bufferToBase64Url(att.attestationObject),
      clientDataJSON: bufferToBase64Url(att.clientDataJSON),
    },
    clientExtensionResults: cred.getClientExtensionResults(),
  };
}

function encodeAssertion(cred: PublicKeyCredential) {
  const assertion = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: bufferToBase64Url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bufferToBase64Url(assertion.authenticatorData),
      clientDataJSON: bufferToBase64Url(assertion.clientDataJSON),
      signature: bufferToBase64Url(assertion.signature),
      userHandle: assertion.userHandle ? bufferToBase64Url(assertion.userHandle) : null,
    },
    clientExtensionResults: cred.getClientExtensionResults(),
  };
}

// doorman returns a stable machine error code in `{ "error": { "code, message } }`.
// `AuthError` carries that code so the UI can map it to product copy instead of
// showing the raw JSON.
export class AuthError extends GameboxdError {
  constructor(
    code: AnyErrorCode | null,
    status: number,
    readonly guidance: string,
  ) {
    super(status, code, guidance);
    this.name = "AuthError";
  }
}

let baseUrl = "";

/**
 * Points every call at the backend's origin. Required in this app: the SPA is
 * served from `FRONTEND_ORIGIN` and the API from somewhere else, so a relative
 * path would resolve against the SPA. Pass the origin without a trailing slash
 * (for example `http://localhost:3000`).
 *
 * `credentials: "include"` below is what carries the session cookie across
 * that origin boundary, and the backend's CORS layer names this exact origin.
 */
export function configureAuth(options: { baseUrl: string }): void {
  baseUrl = options.baseUrl.replace(/\/$/, "");
}

async function requestJson<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(baseUrl + path, { credentials: "include", ...init });
  const text = await res.text();
  if (!res.ok) {
    let code: AnyErrorCode | null = null;
    try {
      // The wire carries a bare string. Doorman's set is pinned by
      // `tests/contract.rs`, so a value outside `AuthCode` means the pin moved
      // without this contract being updated — `guidanceFor` still falls back
      // sensibly, and the drift test is what actually reports it.
      code =
        ((JSON.parse(text) as { error?: { code?: string } }).error?.code as
          | AnyErrorCode
          | undefined) ?? null;
    } catch {
      // non-JSON body (e.g. the CSRF layer); fall back to status-based guidance.
    }
    throw new AuthError(code, res.status, guidanceFor(code, res.status));
  }
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

async function getJson<T>(path: string): Promise<T> {
  return requestJson<T>(path);
}

async function postJson<T>(path: string, body?: unknown): Promise<T> {
  return requestJson<T>(path, {
    method: "POST",
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

async function deleteJson(path: string, body?: unknown): Promise<void> {
  await requestJson<void>(path, {
    method: "DELETE",
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/**
 * Runs one WebAuthn ceremony and turns its failures into `AuthError`s.
 *
 * This is the part upstream's reference leaves to the caller. Both
 * `navigator.credentials.create()` and `.get()` signal every outcome except
 * success by *rejecting* with a `DOMException` — they do not resolve `null`,
 * which is why the `if (!cred)` guard the reference carries almost never fires.
 * Letting those reject raw means the single most common outcome of a passkey
 * prompt, the user dismissing it, arrives at the UI as an unrecognised
 * exception with a browser-authored message.
 *
 * The `null` case is still handled: it is reachable through an aborted
 * conditional-mediation request, and it means the same thing as a dismissal.
 *
 * `status` is 0 on all of these. No HTTP request failed — the browser refused
 * or the user declined before anything was sent — and inventing a status would
 * let a caller believe it matched a response the server never produced.
 */
async function ceremony(run: () => Promise<Credential | null>): Promise<PublicKeyCredential> {
  // An insecure context (plain HTTP on anything but localhost) or a browser
  // without WebAuthn leaves this undefined, and calling through it would throw
  // a TypeError that says nothing a user could act on.
  if (typeof PublicKeyCredential === "undefined" || !navigator.credentials) {
    throw ceremonyFailure("passkey_unsupported");
  }
  let credential: Credential | null;
  try {
    credential = await run();
  } catch (error) {
    throw ceremonyFailure(codeForCeremonyFailure(error));
  }
  if (!credential) throw ceremonyFailure("passkey_cancelled");
  return credential as PublicKeyCredential;
}

function ceremonyFailure(code: ClientCode | "webauthn_error"): AuthError {
  return new AuthError(code, 0, guidanceFor(code, 0));
}

function codeForCeremonyFailure(error: unknown): ClientCode | "webauthn_error" {
  if (!(error instanceof DOMException)) return "webauthn_error";
  switch (error.name) {
    // WebAuthn reports a dismissed prompt and an expired one identically, by
    // design: distinguishing them would tell a caller whether a credential
    // exists on the authenticator. They are one case here for the same reason.
    case "NotAllowedError":
    case "AbortError":
    case "TimeoutError":
      return "passkey_cancelled";
    // Registration only, and not really a failure: `excludeCredentials` matched,
    // so this authenticator already holds a passkey for this account. The user
    // should sign in, not enrol again.
    case "InvalidStateError":
      return "passkey_already_registered";
    case "NotSupportedError":
    case "ConstraintError":
      return "passkey_unsupported";
    // The page's origin does not match the relying-party ID the server issued.
    // In practice this is a misconfiguration — `RP_ID` against `FRONTEND_ORIGIN`
    // — and `Config::validate` in `src/config.rs` is what prevents it server
    // side; this is the browser telling us it got through anyway.
    case "SecurityError":
      return "passkey_origin_mismatch";
    default:
      return "webauthn_error";
  }
}

async function finishRegistration<T>(
  start: Challenge<ServerCreationOptions>,
  finishPath: string,
  name?: string,
): Promise<T> {
  const cred = await ceremony(() => navigator.credentials.create(decodeCreationOptions(start.options)));
  return postJson<T>(finishPath, {
    challenge_id: start.challenge_id,
    response: encodeAttestation(cred),
    name,
  });
}

export async function requestSignup(email: string): Promise<void> {
  await postJson<void>("/auth/signup/request", { email });
}

export async function finishSignup(token: string, passkeyName?: string): Promise<AuthResponse> {
  const start = await postJson<Challenge<ServerCreationOptions>>("/auth/signup/consume", { token });
  return finishRegistration<AuthResponse>(start, "/auth/signup/finish", passkeyName);
}

export async function requestRecovery(email: string): Promise<void> {
  await postJson<void>("/auth/recovery/request", { email });
}

export async function finishRecovery(token: string, passkeyName?: string): Promise<AuthResponse> {
  const start = await postJson<Challenge<ServerCreationOptions>>("/auth/recovery/consume", { token });
  return finishRegistration<AuthResponse>(start, "/auth/recovery/finish", passkeyName);
}

export async function registerPasskey(name?: string): Promise<PasskeyDto> {
  const start = await postJson<Challenge<ServerCreationOptions>>("/auth/passkey/register/start");
  return finishRegistration<PasskeyDto>(start, "/auth/passkey/register/finish", name);
}

export async function authenticatePasskey(): Promise<AuthResponse> {
  const start = await postJson<Challenge<ServerRequestOptions>>("/auth/passkey/authenticate/start");
  const cred = await ceremony(() => navigator.credentials.get(decodeRequestOptions(start.options)));
  return postJson<AuthResponse>("/auth/passkey/authenticate/finish", {
    challenge_id: start.challenge_id,
    response: encodeAssertion(cred),
  });
}

export async function listPasskeys(): Promise<PasskeyDto[]> {
  return getJson<PasskeyDto[]>("/auth/passkey");
}

type Reauthentication = { challenge_id: string; assertion: unknown };

/**
 * Runs the step-up ceremony and returns the proof a gated action must carry.
 *
 * Doorman exposes only the `start` half: the assertion is verified by the
 * action being retried, not by a `finish` route of its own.
 */
export async function startReauth(): Promise<Reauthentication> {
  const start = await postJson<Challenge<ServerRequestOptions>>("/auth/reauth/start");
  const cred = await ceremony(() => navigator.credentials.get(decodeRequestOptions(start.options)));
  return { challenge_id: start.challenge_id, assertion: encodeAssertion(cred) };
}

/**
 * Performs a destructive action, stepping up once if the server asks.
 *
 * The first attempt deliberately carries no proof: whether an action is gated
 * is the server's policy, and asking the user for their passkey before knowing
 * it is needed would be a prompt for nothing. Only one retry is made — a second
 * `reauth_required` after presenting a fresh assertion is a real failure, not
 * something to loop on.
 */
async function withReauth<T>(action: (proof?: Reauthentication) => Promise<T>): Promise<T> {
  try {
    return await action();
  } catch (error) {
    if (!isReauthRequired(error)) throw error;
    return action(await startReauth());
  }
}

// Removing a passkey is gated: it is how an attacker with a live session would
// lock the owner out.
export async function deletePasskey(credentialId: string): Promise<void> {
  await withReauth((reauthentication) =>
    deleteJson(`/auth/passkey/${credentialId}`, reauthentication && { reauthentication }),
  );
}

export async function listSessions(): Promise<SessionDto[]> {
  return getJson<SessionDto[]>("/auth/sessions");
}

export async function revokeSession(id: string): Promise<void> {
  await deleteJson(`/auth/sessions/${id}`);
}

export async function revokeOtherSessions(): Promise<void> {
  await deleteJson("/auth/sessions");
}

// End the current session server-side; the server also clears the cookie.
export async function logout(): Promise<void> {
  await postJson<void>("/auth/logout");
}

// Revoke every session for the user, including this one.
export async function logoutEverywhere(): Promise<void> {
  await postJson<void>("/auth/logout-all");
}

// Start a verified email change. Mails a confirmation link to the new address;
// the account email does not change until `confirmEmailChange` redeems the
// token. Blocked during the recovery cool-down.
export async function requestEmailChange(newEmail: string): Promise<void> {
  await withReauth((reauthentication) =>
    postJson<void>("/auth/account/email", { new_email: newEmail, reauthentication }),
  );
}

// Redeem the token from the new address's link. No session required — the
// single-use token authorises the change. Returns the updated user.
export async function confirmEmailChange(token: string): Promise<UserDto> {
  return postJson<UserDto>("/auth/account/email/confirm", { token });
}

// Irreversibly delete the current account and everything tied to it; the
// server also clears the cookie. Blocked during the recovery cool-down.
export async function deleteAccount(): Promise<void> {
  await withReauth((reauthentication) =>
    deleteJson("/auth/account", reauthentication && { reauthentication }),
  );
}
