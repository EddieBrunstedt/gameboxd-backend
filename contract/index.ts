// The Gameboxd frontend contract: generated wire types, a typed client for the
// application routes, and the browser adapter for Doorman's passkey flows.
//
// See ./README.md for what is generated, what is hand-written, and how each
// half is kept from drifting away from the server.

// `errors` first: both clients re-export parts of it, and a single source for
// the shared names keeps the star exports from colliding.
export * from "./errors";
export * from "./client";
export * from "./auth";

export type { CatalogGame } from "./types/CatalogGame";
export type { CatalogPage } from "./types/CatalogPage";
export type { CreatePlaythroughInput } from "./types/CreatePlaythroughInput";
export type { EntryInput } from "./types/EntryInput";
export type { ErrorBody } from "./types/ErrorBody";
export type { ErrorCode } from "./types/ErrorCode";
export type { ErrorPayload } from "./types/ErrorPayload";
export type { FieldError } from "./types/FieldError";
export type { FieldErrorCode } from "./types/FieldErrorCode";
export type { Game } from "./types/Game";
export type { LibraryEntry } from "./types/LibraryEntry";
export type { LibraryRow } from "./types/LibraryRow";
export type { Me } from "./types/Me";
export type { NamedItem } from "./types/NamedItem";
export type { Page } from "./types/Page";
export type { PlayStatus } from "./types/PlayStatus";
export type { Playthrough } from "./types/Playthrough";
export type { PlaythroughInput } from "./types/PlaythroughInput";
export type { PlaythroughPatch } from "./types/PlaythroughPatch";
export type { Profile } from "./types/Profile";
export type { ProfileInput } from "./types/ProfileInput";

import { configureApi } from "./client";
import { configureAuth } from "./auth";

/**
 * Points both halves of the contract at the backend. Call once at startup.
 *
 * The SPA and the API are separate origins, so nothing here works on relative
 * paths, and the backend's CORS layer only names the origin configured as
 * `FRONTEND_ORIGIN`.
 */
export function configureGameboxd(options: { baseUrl: string }): void {
  configureApi(options);
  configureAuth(options);
}

export type { PlaythroughState } from "./types/PlaythroughState";
export type { StartPlaythroughInput } from "./types/StartPlaythroughInput";
export type { LifecycleAction } from "./types/LifecycleAction";
export type { LifecycleInput } from "./types/LifecycleInput";

export type { DiaryNote } from "./types/DiaryNote";
export type { NoteInput } from "./types/NoteInput";
export type { NotePatch } from "./types/NotePatch";

export type { PublicationInput } from "./types/PublicationInput";
export type { VisibilityInput } from "./types/VisibilityInput";
export type { PlaythroughVisibilityInput } from "./types/PlaythroughVisibilityInput";
export type { PublicLibraryEntry } from "./types/PublicLibraryEntry";
export type { PublicPlaythrough } from "./types/PublicPlaythrough";
export type { PublicRating } from "./types/PublicRating";
export type { PublicReview } from "./types/PublicReview";
export type { PublicNote } from "./types/PublicNote";
export type { Visibility } from "./types/Visibility";
export type { PublicGame } from "./types/PublicGame";

export type { HandleInput } from "./types/HandleInput";
export type { PublicProfile } from "./types/PublicProfile";
export type { PublicProfileCounts } from "./types/PublicProfileCounts";
export type { ProfileSection } from "./types/ProfileSection";
export type { PublicProfileSection } from "./types/PublicProfileSection";

export type { BrowsePage } from "./types/BrowsePage";
export type { BrowseQuery } from "./types/BrowseQuery";
export type { StatusCounts } from "./types/StatusCounts";
export type { LibrarySort } from "./types/LibrarySort";

export type { GameList } from "./types/GameList";
export type { GameListDetail } from "./types/GameListDetail";
export type { PublicList } from "./types/PublicList";
export type { PublicListDetail } from "./types/PublicListDetail";
export type { CreateListInput } from "./types/CreateListInput";
export type { ListInput } from "./types/ListInput";
export type { ListGamesInput } from "./types/ListGamesInput";
export type { ListKind } from "./types/ListKind";

export type { Statistics } from "./types/Statistics";
export type { StatisticsQuery } from "./types/StatisticsQuery";
export type { PlaythroughStatistics } from "./types/PlaythroughStatistics";
export type { RatingBucket } from "./types/RatingBucket";

export type { FeedItem } from "./types/FeedItem";
export type { FeedQuery } from "./types/FeedQuery";
export type { FollowedProfile } from "./types/FollowedProfile";
export type { FollowingQuery } from "./types/FollowingQuery";
export type { SocialPage } from "./types/SocialPage";
