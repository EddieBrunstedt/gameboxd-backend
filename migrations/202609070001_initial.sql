-- Doorman owns gameboxd_auth. Its pinned PostgreSQL adapter uses BYTEA user IDs.
-- This is the only deliberate schema coupling: account deletion cascades to app data.
CREATE TABLE profiles (
    user_id BYTEA PRIMARY KEY REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL DEFAULT '' CHECK (char_length(display_name) <= 80),
    bio TEXT NOT NULL DEFAULT '' CHECK (char_length(bio) <= 1000),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE games (
    id UUID PRIMARY KEY,
    igdb_id BIGINT NOT NULL UNIQUE CHECK (igdb_id > 0),
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    summary TEXT,
    cover_url TEXT,
    first_release_date TIMESTAMPTZ,
    platforms JSONB NOT NULL DEFAULT '[]',
    genres JSONB NOT NULL DEFAULT '[]',
    developers JSONB NOT NULL DEFAULT '[]',
    game_type BIGINT,
    parent_igdb_id BIGINT,
    version_parent_igdb_id BIGINT,
    source_url TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    refreshed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX games_name_idx ON games (lower(name), id);

CREATE TABLE library_entries (
    user_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    game_id UUID NOT NULL REFERENCES games(id),
    status TEXT NOT NULL CHECK (status IN ('backlog','playing','completed','on_hold','dropped')),
    rating SMALLINT CHECK (rating BETWEEN 1 AND 10),
    review TEXT CHECK (char_length(review) <= 10000),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, game_id)
);
CREATE INDEX library_recent_idx ON library_entries (user_id, updated_at DESC, game_id);

CREATE TABLE auth_audit_events (
    id BIGSERIAL PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL,
    event JSONB NOT NULL
);
CREATE INDEX auth_audit_time_idx ON auth_audit_events (occurred_at);
