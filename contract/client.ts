import type { ModerationTarget } from "./types/ModerationTarget";
import type { ReportInput } from "./types/ReportInput";
import type { ReportReceipt } from "./types/ReportReceipt";
import type { BlockedProfile } from "./types/BlockedProfile";
import type { OwnerRestriction } from "./types/OwnerRestriction";
import type { ModerationRestriction } from "./types/ModerationRestriction";
import type { RestrictionInput } from "./types/RestrictionInput";
import type { ReportResolutionInput } from "./types/ReportResolutionInput";
import type { ModerationReport } from "./types/ModerationReport";
import type { ModerationAudit } from "./types/ModerationAudit";
import type { ModerationQuery } from "./types/ModerationQuery";
import type { FeedItem } from "./types/FeedItem";
import type { FeedQuery } from "./types/FeedQuery";
import type { FollowedProfile } from "./types/FollowedProfile";
import type { FollowingQuery } from "./types/FollowingQuery";
import type { SocialPage } from "./types/SocialPage";
import type { Statistics } from "./types/Statistics";
import type { StatisticsQuery } from "./types/StatisticsQuery";
import type { GameList } from "./types/GameList";
import type { GameListDetail } from "./types/GameListDetail";
import type { PublicList } from "./types/PublicList";
import type { PublicListDetail } from "./types/PublicListDetail";
import type { CreateListInput } from "./types/CreateListInput";
import type { ListInput } from "./types/ListInput";
import type { ListGamesInput } from "./types/ListGamesInput";
import type { BrowseQuery } from "./types/BrowseQuery";
import type { BrowsePage } from "./types/BrowsePage";
import type { PublicProfileSection } from "./types/PublicProfileSection";
import type { ProfileSection } from "./types/ProfileSection";
import type { PublicProfile } from "./types/PublicProfile";
import type { HandleInput } from "./types/HandleInput";
import type { PublicationInput } from "./types/PublicationInput";
import type { VisibilityInput } from "./types/VisibilityInput";
import type { PlaythroughVisibilityInput } from "./types/PlaythroughVisibilityInput";
import type { PublicLibraryEntry } from "./types/PublicLibraryEntry";
import type { PublicPlaythrough } from "./types/PublicPlaythrough";
import type { PublicRating } from "./types/PublicRating";
import type { PublicReview } from "./types/PublicReview";
import type { PublicNote } from "./types/PublicNote";
// Typed client for the Gameboxd application routes.
//
// Every request and response type here is imported from `./types`, which is
// generated from the Rust structs that actually serialize the wire format —
// nothing in this file restates a field name, so a renamed or removed field is
// a TypeScript error rather than a runtime surprise.
//
// What *is* written by hand is the route table: paths, methods, and which
// routes carry a precondition. Axum's router erases all of that at the type
// level, so no generator can recover it. It changes rarely, and a mistake here
// fails immediately and loudly as a 404 or 405.
//
// Authentication lives in `./auth.ts`; this file assumes a session cookie is
// already present and sends `credentials: "include"` so the browser attaches it.
//
// It does not, however, own the authentication *errors*. Doorman's session
// layer wraps these routes too, so any of them can reject with a Doorman code —
// `session_not_found` on a missing cookie, `csrf_origin_mismatch` on a bad
// Origin. Those codes and the predicates for them live in `./errors.ts`,
// shared with `./auth.ts`, because a screen catching a 401 from `me()` needs
// the same branch as one catching it from `logout()`.

import type { DiaryNote } from "./types/DiaryNote";
import type { NoteInput } from "./types/NoteInput";
import type { NotePatch } from "./types/NotePatch";
import type { CatalogGame } from "./types/CatalogGame";
import type { CatalogPage } from "./types/CatalogPage";
import type { CreatePlaythroughInput } from "./types/CreatePlaythroughInput";
import type { StartPlaythroughInput } from "./types/StartPlaythroughInput";
import type { LifecycleInput } from "./types/LifecycleInput";
import type { EntryInput } from "./types/EntryInput";
import type { ErrorBody } from "./types/ErrorBody";
import type { FieldError } from "./types/FieldError";
import type { Game } from "./types/Game";
import type { LibraryEntry } from "./types/LibraryEntry";
import type { LibraryRow } from "./types/LibraryRow";
import type { Me } from "./types/Me";
import type { Page } from "./types/Page";
import type { Playthrough } from "./types/Playthrough";
import type { PlaythroughInput } from "./types/PlaythroughInput";
import type { PlaythroughPatch } from "./types/PlaythroughPatch";
import type { Profile } from "./types/Profile";
import type { ProfileInput } from "./types/ProfileInput";

import { type AnyErrorCode, GameboxdError } from "./errors";

let baseUrl = "";

/**
 * Points every call at the backend's origin, which is not the SPA's. Pass it
 * without a trailing slash, for example `http://localhost:3000`.
 */
export function configureApi(options: { baseUrl: string }): void {
  baseUrl = options.baseUrl.replace(/\/$/, "");
}

/**
 * A non-2xx response from an application route.
 *
 * `code` spans two vocabularies, because these routes really do answer with
 * both: `ErrorCode` for anything the application decided, and Doorman's codes
 * for anything its session or CSRF layer decided before the handler ran. Typing
 * it as `ErrorCode` alone made the sign-out branch — the single most common
 * thing a screen does with an error — a comparison TypeScript rejected.
 *
 * `fields` is present exactly when individual fields were rejected — by our own
 * validation or by the JSON schema — and is the thing a form should branch on,
 * since both kinds arrive the same way. Only application validation produces
 * it, so it is always empty on an error that came from the auth layers.
 */
export class ApiError extends GameboxdError {
  constructor(
    status: number,
    code: AnyErrorCode | null,
    message: string,
    readonly fields: readonly FieldError[] = [],
  ) {
    super(status, code, message);
    this.name = "ApiError";
  }

  /** The rejections keyed by field name, for a form that renders per control. */
  byField(): Record<string, FieldError> {
    return Object.fromEntries(this.fields.map((field) => [field.field, field]));
  }
}

/**
 * True when a playthrough or note edit lost a race and must be reloaded and reapplied.
 * See `edit` below for why every edit carries a precondition in the first place.
 */
export function isStale(error: unknown): boolean {
  return error instanceof ApiError && error.code === "precondition_failed";
}

// Re-exported so a screen that only imports the application client still gets
// the auth branches it cannot avoid needing.
export {
  GameboxdError,
  isAccountDisabled,
  isReauthRequired,
  isRecoveryCooldown,
  isSignedOut,
} from "./errors";
export type { AnyErrorCode, AuthCode, ClientCode } from "./errors";

type Query = Record<string, string | number | boolean | undefined | null>;

function url(path: string, query?: Query): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(query ?? {})) {
    if (value !== undefined && value !== null) search.set(key, String(value));
  }
  const suffix = search.toString();
  return `${baseUrl}${path}${suffix ? `?${suffix}` : ""}`;
}

async function request<T>(
  method: string,
  path: string,
  options: { query?: Query; body?: unknown; ifMatch?: string } = {},
): Promise<T> {
  const headers: Record<string, string> = {};
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  if (options.ifMatch !== undefined) headers["If-Match"] = options.ifMatch;
  const response = await fetch(url(path, options.query), {
    method,
    credentials: "include",
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  });
  const text = await response.text();
  if (!response.ok) throw toError(response.status, text);
  // 204 responses, and only those, have no body.
  return (text ? JSON.parse(text) : undefined) as T;
}

function toError(status: number, text: string): ApiError {
  try {
    // `ErrorBody` is generated from the application's envelope, so it types
    // `code` as `ErrorCode`. Doorman reuses the envelope's shape but not its
    // code list, so the runtime value is wider than that type admits; widening
    // it into `AnyErrorCode` here is what makes the auth branches expressible.
    const { error } = JSON.parse(text) as ErrorBody;
    return new ApiError(status, error.code, error.message, error.fields ?? []);
  } catch {
    // A proxy, a CORS failure, or anything else that never reached the app's
    // own error handling. There is no code to report, and inventing one would
    // let a caller believe it matched a case the backend never produced.
    return new ApiError(status, null, `Request failed with status ${status}`);
  }
}

/**
 * The precondition a playthrough edit must carry.
 *
 * Read from the record itself rather than from the `ETag` header: the backend
 * publishes `version` in every playthrough body, including inside listings, so
 * an editor opened from a list needs no extra request to learn its revision.
 */
function precondition(playthrough: Pick<Playthrough, "version">): string {
  return `"${playthrough.version}"`;
}

// --- Identity and profile -------------------------------------------------

export const me = (): Promise<Me> => request("GET", "/api/me");

export const updateProfile = (input: ProfileInput): Promise<Profile> =>
  request("PUT", "/api/me", { body: input });

// --- Catalog --------------------------------------------------------------

/** Searches IGDB. Public, read-only, and stores nothing locally. */
export const searchCatalog = (
  q: string,
  page: { limit?: number; offset?: number } = {},
): Promise<CatalogPage<CatalogGame>> =>
  request("GET", "/api/catalog/search", { query: { q, ...page } });

/**
 * One IGDB game, for a detail page. Works without a prior search, without an
 * import, and without a session, so a direct link or a reload is enough.
 */
export const catalogGame = (igdbId: number): Promise<CatalogGame> =>
  request("GET", `/api/catalog/games/${igdbId}`);

/** Games already stored locally. Does not contact IGDB. */
export const games = (
  query: { q?: string; limit?: number; offset?: number } = {},
): Promise<Page<CatalogGame>> => request("GET", "/api/games", { query });

export const game = (igdbId: number): Promise<CatalogGame> => request("GET", `/api/games/${igdbId}`);

export const importGame = (igdbId: number): Promise<Game> =>
  request("POST", `/api/catalog/import/${igdbId}`);

/** Query capabilities are documented per public section in docs/api.md. */
export type LibraryQuery = Pick<BrowseQuery, "limit" | "offset" | "q" | "igdb_id" | "status" | "sort" | "has_rating">;
export type PlaythroughQuery = Omit<BrowseQuery, "status" | "sort" | "text">;
export type NoteQuery = Omit<BrowseQuery, "status" | "sort">;

// --- Library --------------------------------------------------------------

export const library = (
  query: LibraryQuery = {},
): Promise<BrowsePage<LibraryRow>> => request("GET", "/api/me/library", { query });

export const libraryEntry = (igdbId: number): Promise<LibraryEntry> =>
  request("GET", `/api/me/library/${igdbId}`);

/**
 * Sets the library status for a game, storing the game if it is not known yet.
 *
 * `status` is the only writable field: the entry's `rating` is derived from
 * playthroughs and is edited there. That is also why this route carries no
 * precondition. A conflicting status while a current run exists returns 409;
 * use transitionPlaythrough to change that run and status together.
 */
export const saveLibraryEntry = (igdbId: number, input: EntryInput): Promise<LibraryEntry> =>
  request("PUT", `/api/me/library/${igdbId}`, { body: input });

/** Removes library membership. Playthroughs for the game are preserved. */
export const removeLibraryEntry = (igdbId: number): Promise<void> =>
  request("DELETE", `/api/me/library/${igdbId}`);

// --- Playthroughs ---------------------------------------------------------

export const playthroughs = (
  query: PlaythroughQuery = {},
): Promise<BrowsePage<Playthrough>> => request("GET", "/api/me/playthroughs", { query });

export const playthrough = (id: string): Promise<Playthrough> =>
  request("GET", `/api/me/playthroughs/${id}`);

/** Starts a current run (or replay), atomically setting library status to playing. */
export const startPlaythrough = (input: StartPlaythroughInput): Promise<Playthrough> =>
  request("POST", "/api/me/playthroughs/start", { body: input });

/** Pauses, resumes, completes or abandons the current run and updates library status. */
export const transitionPlaythrough = (
  current: Playthrough,
  input: LifecycleInput,
): Promise<Playthrough> =>
  request("POST", `/api/me/playthroughs/${current.id}/lifecycle`, {
    body: input,
    ifMatch: precondition(current),
  });

/** Records history without changing present-day library status or the current run. */
export const createPlaythrough = (input: CreatePlaythroughInput): Promise<Playthrough> =>
  request("POST", "/api/me/playthroughs", { body: input });

/**
 * Replaces every editable field. An omitted field is cleared, exactly as an
 * explicit `null` is — use it only where one form owns the whole record.
 */
export const replacePlaythrough = (
  current: Playthrough,
  input: PlaythroughInput,
): Promise<Playthrough> =>
  request("PUT", `/api/me/playthroughs/${current.id}`, {
    body: input,
    ifMatch: precondition(current),
  });

/**
 * Changes only the fields present. An omitted field keeps its stored value and
 * an explicit `null` clears it, so a rating widget, a review editor and a date
 * picker can each send only what they own.
 *
 * The precondition is mandatory server-side, which is why it is taken from the
 * record here rather than offered as an option: an edit that cannot say which
 * revision it read is an edit that can overwrite work the user never saw.
 */
export const updatePlaythrough = (
  current: Playthrough,
  patch: PlaythroughPatch,
): Promise<Playthrough> =>
  request("PATCH", `/api/me/playthroughs/${current.id}`, {
    body: patch,
    ifMatch: precondition(current),
  });

/**
 * Deletes a playthrough. Passing the record asserts its revision; passing an id
 * alone is unconditional and stays idempotent.
 */
export const deletePlaythrough = (target: Playthrough | string): Promise<void> =>
  typeof target === "string"
    ? request("DELETE", `/api/me/playthroughs/${target}`)
    : request("DELETE", `/api/me/playthroughs/${target.id}`, {
        ifMatch: precondition(target),
      });

// --- Private diary notes --------------------------------------------------

/** All of the owner's notes, ordered by chosen day, unknown dates last. */
export const diary = (
  query: NoteQuery = {},
): Promise<BrowsePage<DiaryNote>> => request("GET", "/api/me/diary", { query });

export const playthroughNotes = (
  playthroughId: string,
  query: NoteQuery = {},
): Promise<BrowsePage<DiaryNote>> =>
  request("GET", `/api/me/playthroughs/${playthroughId}/notes`, { query });

export const note = (id: string): Promise<DiaryNote> =>
  request("GET", `/api/me/notes/${id}`);

export const createNote = (playthroughId: string, input: NoteInput): Promise<DiaryNote> =>
  request("POST", `/api/me/playthroughs/${playthroughId}/notes`, { body: input });

/** Replaces text and chosen day; omission of the day clears it. */
export const replaceNote = (current: DiaryNote, input: NoteInput): Promise<DiaryNote> =>
  request("PUT", `/api/me/notes/${current.id}`, { body: input, ifMatch: precondition(current) });

/** Omitted fields are preserved; only presentation_on accepts null. */
export const updateNote = (current: DiaryNote, patch: NotePatch): Promise<DiaryNote> =>
  request("PATCH", `/api/me/notes/${current.id}`, { body: patch, ifMatch: precondition(current) });

/** Deletion requires the note's revision, just like an edit. */
export const deleteNote = (current: DiaryNote): Promise<void> =>
  request("DELETE", `/api/me/notes/${current.id}`, { ifMatch: precondition(current) });


// --- Publication controls and public projections --------------------------

/** Profile publication gates every public item without changing item choices. */
export const setPublication = (current: Profile, input: PublicationInput): Promise<Profile> =>
  request("PUT", "/api/me/publication", { body: input, ifMatch: precondition(current) });

export const setLibraryVisibility = (current: LibraryEntry, input: VisibilityInput): Promise<LibraryEntry> =>
  request("PUT", `/api/me/library/${current.igdb_id}/visibility`, { body: input, ifMatch: precondition(current) });

export const setPlaythroughVisibility = (current: Playthrough, input: PlaythroughVisibilityInput): Promise<Playthrough> =>
  request("PUT", `/api/me/playthroughs/${current.id}/visibility`, { body: input, ifMatch: precondition(current) });

export const setNoteVisibility = (current: DiaryNote, input: VisibilityInput): Promise<DiaryNote> =>
  request("PUT", `/api/me/notes/${current.id}/visibility`, { body: input, ifMatch: precondition(current) });

/** These reads apply identical visibility rules with or without a session. */
export const publicLibraryEntry = (id: string): Promise<PublicLibraryEntry> =>
  request("GET", `/api/public/library/${encodeURIComponent(id)}`);
export const publicPlaythrough = (id: string): Promise<PublicPlaythrough> =>
  request("GET", `/api/public/playthroughs/${encodeURIComponent(id)}`);
export const publicRating = (id: string): Promise<PublicRating> =>
  request("GET", `/api/public/ratings/${encodeURIComponent(id)}`);
export const publicReview = (id: string): Promise<PublicReview> =>
  request("GET", `/api/public/reviews/${encodeURIComponent(id)}`);
export const publicNote = (id: string): Promise<PublicNote> =>
  request("GET", `/api/public/notes/${encodeURIComponent(id)}`);

// --- Public profiles and authenticated visitor preview ---------------------

export const setHandle = (current: Profile, input: HandleInput): Promise<Profile> =>
  request("PUT", "/api/me/handle", { body: input, ifMatch: precondition(current) });

export const publicProfile = (id: string): Promise<PublicProfile> =>
  request("GET", `/api/public/profiles/${encodeURIComponent(id)}`);

/** Resolves a current handle to the canonical profile; retired names return 404. */
export const publicProfileByHandle = (handle: string): Promise<PublicProfile> =>
  request("GET", `/api/public/handles/${encodeURIComponent(handle)}`);

export const publicProfileSection = (
  id: string, section: ProfileSection, query?: BrowseQuery,
): Promise<PublicProfileSection> =>
  request("GET", `/api/public/profiles/${encodeURIComponent(id)}/${section}`, { query });

/** Uses the session owner's item choices, even while their profile is disabled. */
export const publicPreview = (): Promise<PublicProfile> =>
  request("GET", "/api/me/public-preview");

export const publicPreviewSection = (
  section: ProfileSection, query?: BrowseQuery,
): Promise<PublicProfileSection> =>
  request("GET", `/api/me/public-preview/${section}`, { query });

// --- Ordered lists and favorites ------------------------------------------

export type ListQuery = Pick<BrowseQuery, "limit" | "offset" | "q">;

/** q searches list titles; order is title, then stable list ID. */
export const lists = (query: ListQuery = {}): Promise<BrowsePage<GameList>> =>
  request("GET", "/api/me/lists", { query });

export const list = (id: string): Promise<GameListDetail> =>
  request("GET", `/api/me/lists/${encodeURIComponent(id)}`);

/** Empty and private initially; at most one kind=favorites list per owner. */
export const createList = (input: CreateListInput): Promise<GameListDetail> =>
  request("POST", "/api/me/lists", { body: input });

/** Replaces title/description. Omitted description becomes empty. */
export const replaceList = (current: GameList, input: ListInput): Promise<GameListDetail> =>
  request("PUT", `/api/me/lists/${current.id}`, { body: input, ifMatch: precondition(current) });

/**
 * Adds, removes and reorders by replacing the complete sequence (max 500).
 * Import unknown games first. A 412 requires rereading and reconciling changes.
 * Editing a public list explicitly shares its new membership immediately.
 */
export const setListGames = (current: GameList, input: ListGamesInput): Promise<GameListDetail> =>
  request("PUT", `/api/me/lists/${current.id}/games`, { body: input, ifMatch: precondition(current) });

export const setListVisibility = (current: GameList, input: VisibilityInput): Promise<GameListDetail> =>
  request("PUT", `/api/me/lists/${current.id}/visibility`, { body: input, ifMatch: precondition(current) });

export const deleteList = (current: GameList): Promise<void> =>
  request("DELETE", `/api/me/lists/${current.id}`, { ifMatch: precondition(current) });

export const publicList = (id: string): Promise<PublicListDetail> =>
  request("GET", `/api/public/lists/${encodeURIComponent(id)}`);

export const publicLists = (profileId: string, query: ListQuery = {}): Promise<PublicProfileSection> =>
  publicProfileSection(profileId, "lists", query);

/** Selected membership only, with the authenticated owner's profile gate bypassed. */
export const previewList = (id: string): Promise<PublicListDetail> =>
  request("GET", `/api/me/public-preview/lists/${encodeURIComponent(id)}`);

// --- Statistics -----------------------------------------------------------

/** Date filters use chosen historical dates; status counts remain current. */
export const statistics = (query: StatisticsQuery = {}): Promise<Statistics> =>
  request("GET", "/api/me/statistics", { query });

/** Requires explicit statistics publication and an enabled profile. */
export const publicStatistics = (profileId: string, query: StatisticsQuery = {}): Promise<Statistics> =>
  request("GET", `/api/public/profiles/${encodeURIComponent(profileId)}/statistics`, { query });

/** Applies the summary choice and public inputs, bypassing only the profile gate. */
export const previewStatistics = (query: StatisticsQuery = {}): Promise<Statistics> =>
  request("GET", "/api/me/public-preview/statistics", { query });

// --- Private following and historical feed --------------------------------

export const following = (query: FollowingQuery = {}): Promise<SocialPage<FollowedProfile>> =>
  request("GET", "/api/me/following", { query });

/** Idempotent; target must be public. Your own profile can remain private. */
export const follow = (profileId: string): Promise<void> =>
  request("PUT", `/api/me/following/${encodeURIComponent(profileId)}`);

/** Also succeeds for disabled, deleted, or already-unfollowed profiles. */
export const unfollow = (profileId: string): Promise<void> =>
  request("DELETE", `/api/me/following/${encodeURIComponent(profileId)}`);

/** Live public components in chosen-date order; identity is (kind, item.id). */
export const feed = (query: FeedQuery = {}): Promise<SocialPage<FeedItem>> =>
  request("GET", "/api/me/feed", { query });

// --- Blocking, reporting and operator moderation ---------------------------

export const blocks = (query: ModerationQuery = {}): Promise<BlockedProfile[]> =>
  request("GET", "/api/me/blocks", { query });

/** Removes both follow directions; unblocking never restores them. */
export const block = (profileId: string): Promise<void> =>
  request("PUT", `/api/me/blocks/${encodeURIComponent(profileId)}`);

export const unblock = (profileId: string): Promise<void> =>
  request("DELETE", `/api/me/blocks/${encodeURIComponent(profileId)}`);

/** Active restrictions only; no report evidence, reporter identity or operator reasons. */
export const ownRestrictions = (query: ModerationQuery = {}): Promise<OwnerRestriction[]> =>
  request("GET", "/api/me/restrictions", { query });

/** One case per reporter/target, including resolved cases; 20 new cases per 24 hours. */
export const report = (input: ReportInput): Promise<ReportReceipt> =>
  request("POST", "/api/me/reports", { body: input });

// Every operator request requires a live, administrator-provisioned role grant.
export const moderationReports = (query: ModerationQuery = {}): Promise<ModerationReport[]> =>
  request("GET", "/api/operator/reports", { query });

export const moderationReport = (id: string): Promise<ModerationReport> =>
  request("GET", `/api/operator/reports/${encodeURIComponent(id)}`);

export const resolveReport = (id: string, input: ReportResolutionInput): Promise<ModerationReport> =>
  request("PUT", `/api/operator/reports/${encodeURIComponent(id)}`, { body: input });

export const moderationRestrictions = (query: ModerationQuery = {}): Promise<ModerationRestriction[]> =>
  request("GET", "/api/operator/restrictions", { query });

/** expected_version is zero for a new target, otherwise the last fetched version. */
export const setRestriction = (
  kind: ModerationTarget, id: string, input: RestrictionInput,
): Promise<ModerationRestriction> =>
  request("PUT", `/api/operator/restrictions/${kind}/${encodeURIComponent(id)}`, { body: input });

export const moderationAudit = (query: ModerationQuery = {}): Promise<ModerationAudit[]> =>
  request("GET", "/api/operator/audit", { query });
