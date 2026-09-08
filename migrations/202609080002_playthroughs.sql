-- Library status describes the present; playthroughs preserve each experience.
-- History is independent of library membership, but follows account deletion.
CREATE TABLE playthroughs (
    id UUID PRIMARY KEY,
    user_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    game_id UUID NOT NULL REFERENCES games(id),
    rating SMALLINT CHECK (rating BETWEEN 1 AND 10),
    review TEXT CHECK (char_length(review) <= 10000),
    started_on DATE,
    finished_on DATE,
    platform TEXT CHECK (char_length(platform) BETWEEN 1 AND 100 AND platform = btrim(platform)),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (started_on IS NULL OR finished_on IS NULL OR finished_on >= started_on)
);

-- Existing timestamps are record timestamps, not evidence of when someone played.
INSERT INTO playthroughs (id,user_id,game_id,rating,review,created_at,updated_at)
SELECT gen_random_uuid(),user_id,game_id,rating,review,created_at,updated_at
FROM library_entries WHERE rating IS NOT NULL OR review IS NOT NULL;

ALTER TABLE library_entries DROP COLUMN rating, DROP COLUMN review;

-- UTC makes the fallback date independent of the connection's timezone.
CREATE INDEX playthroughs_game_recent_idx ON playthroughs (
    user_id,game_id,
    (COALESCE(finished_on,started_on,(created_at AT TIME ZONE 'UTC')::date)) DESC,
    created_at DESC,id DESC
);
CREATE INDEX playthroughs_diary_recent_idx ON playthroughs (
    user_id,
    (COALESCE(finished_on,started_on,(created_at AT TIME ZONE 'UTC')::date)) DESC,
    created_at DESC,id DESC
);

-- One definition keeps list, detail, and save responses consistent. Unrated
-- playthroughs never hide a rating; editing text never changes recency.
CREATE VIEW library_entry_details AS
SELECT e.user_id,e.game_id,g.igdb_id,e.status,
       latest.rating,latest.id AS rating_playthrough_id,e.created_at,e.updated_at
FROM library_entries e JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT p.id,p.rating FROM playthroughs p
    WHERE p.user_id=e.user_id AND p.game_id=e.game_id AND p.rating IS NOT NULL
    ORDER BY COALESCE(p.finished_on,p.started_on,(p.created_at AT TIME ZONE 'UTC')::date) DESC,
             p.created_at DESC,p.id DESC
    LIMIT 1
) latest ON true;
