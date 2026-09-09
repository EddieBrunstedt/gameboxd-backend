-- An explicit historical day, independent of play dates and private bookkeeping.
-- No default or backfill: existing timestamps are not evidence of this day.
ALTER TABLE playthroughs ADD COLUMN presentation_on DATE;

DROP INDEX playthroughs_game_recent_idx;
DROP INDEX playthroughs_diary_recent_idx;
CREATE INDEX playthroughs_game_recent_idx ON playthroughs (
    user_id,game_id,presentation_on DESC NULLS LAST,id DESC
);
CREATE INDEX playthroughs_diary_recent_idx ON playthroughs (
    user_id,presentation_on DESC NULLS LAST,id DESC
);

-- All owner library representations select the same rated entry. Known dates
-- outrank unknown ones; UUID v4 ties are stable and reveal no write chronology.
CREATE OR REPLACE VIEW library_entry_details AS
SELECT e.user_id,e.game_id,g.igdb_id,e.status,
       latest.rating,latest.id AS rating_playthrough_id,e.created_at,e.updated_at
FROM library_entries e JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT p.id,p.rating FROM playthroughs p
    WHERE p.user_id=e.user_id AND p.game_id=e.game_id AND p.rating IS NOT NULL
    ORDER BY p.presentation_on DESC NULLS LAST,p.id DESC
    LIMIT 1
) latest ON true;
