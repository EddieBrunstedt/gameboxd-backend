-- An end date says nothing about completion. Existing outcomes stay unknown,
-- and no old experience takes control of the user's present library status.
ALTER TABLE playthroughs
    ADD COLUMN state TEXT CHECK (state IN ('active','paused','completed','abandoned')),
    ADD COLUMN is_current BOOLEAN NOT NULL DEFAULT false,
    ADD CONSTRAINT playthrough_current_state CHECK (
        NOT is_current OR (state IS NOT NULL AND state IN ('active','paused'))
    ),
    ADD CONSTRAINT playthrough_open_end CHECK (
        state IS NULL OR state NOT IN ('active','paused') OR finished_on IS NULL
    );

CREATE UNIQUE INDEX playthroughs_one_current_idx
    ON playthroughs (user_id,game_id) WHERE is_current;

CREATE OR REPLACE VIEW library_entry_details AS
SELECT e.user_id,e.game_id,g.igdb_id,e.status,
       latest.rating,latest.id AS rating_playthrough_id,e.created_at,e.updated_at,
       current_run.id AS current_playthrough_id
FROM library_entries e JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT p.id,p.rating FROM playthroughs p
    WHERE p.user_id=e.user_id AND p.game_id=e.game_id AND p.rating IS NOT NULL
    ORDER BY p.presentation_on DESC NULLS LAST,p.id DESC LIMIT 1
) latest ON true
LEFT JOIN playthroughs current_run ON current_run.user_id=e.user_id
    AND current_run.game_id=e.game_id AND current_run.is_current;
