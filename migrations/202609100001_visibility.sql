-- Publication is opt-in at both levels. No existing content is published.
ALTER TABLE profiles
    ADD COLUMN public_enabled BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0);
ALTER TABLE library_entries
    ADD COLUMN public_id UUID NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','public')),
    ADD COLUMN version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0);
ALTER TABLE playthroughs
    ADD COLUMN summary_visibility TEXT NOT NULL DEFAULT 'private' CHECK (summary_visibility IN ('private','public')),
    ADD COLUMN rating_visibility TEXT NOT NULL DEFAULT 'private' CHECK (rating_visibility IN ('private','public')),
    ADD COLUMN review_visibility TEXT NOT NULL DEFAULT 'private' CHECK (review_visibility IN ('private','public')),
    ADD CONSTRAINT public_rating_present CHECK (rating_visibility='private' OR rating IS NOT NULL),
    ADD CONSTRAINT public_review_present CHECK (review_visibility='private' OR review IS NOT NULL);
ALTER TABLE diary_notes
    ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','public'));

-- Reuse the existing real-change revision function for these owner resources.
CREATE TRIGGER profiles_revision BEFORE UPDATE ON profiles
FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION playthrough_revision();
CREATE TRIGGER library_entries_revision BEFORE UPDATE ON library_entries
FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION playthrough_revision();

-- Clearing content also clears its publication choice. Adding a later rating
-- or review must not inherit a deleted item's permission. Runs and notes never
-- inherit publication from parents, library membership, or previous replays.
CREATE FUNCTION clear_missing_publication() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.rating IS NULL THEN NEW.rating_visibility := 'private'; END IF;
    IF NEW.review IS NULL THEN NEW.review_visibility := 'private'; END IF;
    RETURN NEW;
END;
$$;
-- PostgreSQL fires same-kind triggers by name: normalize before revisioning.
CREATE TRIGGER playthroughs_clear_publication BEFORE UPDATE ON playthroughs
FOR EACH ROW EXECUTE FUNCTION clear_missing_publication();

CREATE OR REPLACE VIEW library_entry_details AS
SELECT e.user_id,e.game_id,g.igdb_id,e.status,
       latest.rating,latest.id AS rating_playthrough_id,e.created_at,e.updated_at,
       current_run.id AS current_playthrough_id,e.public_id,e.visibility,e.version
FROM library_entries e JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT p.id,p.rating FROM playthroughs p
    WHERE p.user_id=e.user_id AND p.game_id=e.game_id AND p.rating IS NOT NULL
    ORDER BY p.presentation_on DESC NULLS LAST,p.id DESC LIMIT 1
) latest ON true
LEFT JOIN playthroughs current_run ON current_run.user_id=e.user_id
    AND current_run.game_id=e.game_id AND current_run.is_current;

-- Views freeze SELECT * at creation; append visibility explicitly to preserve
-- the existing owner's note column order.
CREATE OR REPLACE VIEW diary_note_details AS
SELECT n.id,n.user_id,n.playthrough_id,n.text,n.presentation_on,n.version,n.created_at,n.updated_at,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,n.visibility
FROM diary_notes n JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id
JOIN games g ON g.id=p.game_id;

-- These are the shared authorization boundary for public detail, future
-- sections, counts, search and feeds. user_id/game_id are server-only join keys;
-- deliberate Rust DTOs serialize only the safe projection. No owner DTOs here.
CREATE VIEW public_reviews AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.review
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id AND pr.public_enabled
JOIN games g ON g.id=p.game_id
WHERE p.review_visibility='public' AND p.review IS NOT NULL;

CREATE VIEW public_ratings AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.rating
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id AND pr.public_enabled
JOIN games g ON g.id=p.game_id
WHERE p.rating_visibility='public' AND p.rating IS NOT NULL;

CREATE VIEW public_playthroughs AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.state,p.started_on,p.finished_on,p.platform
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id AND pr.public_enabled
JOIN games g ON g.id=p.game_id
WHERE p.summary_visibility='public';

CREATE VIEW public_notes AS
SELECT n.user_id,p.game_id,n.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       n.presentation_on,n.text
FROM diary_notes n JOIN profiles pr ON pr.user_id=n.user_id AND pr.public_enabled
JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id
JOIN games g ON g.id=p.game_id
WHERE n.visibility='public';

CREATE VIEW public_library_entries AS
SELECT e.user_id,e.game_id,e.public_id AS id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,e.status,
       latest.rating,latest.id AS rating_id
FROM library_entries e JOIN profiles pr ON pr.user_id=e.user_id AND pr.public_enabled
JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT r.id,r.rating FROM public_ratings r
    WHERE r.user_id=e.user_id AND r.game_id=e.game_id
    ORDER BY r.presentation_on DESC NULLS LAST,r.id DESC LIMIT 1
) latest ON true
WHERE e.visibility='public';

CREATE INDEX public_reviews_owner_date_idx ON playthroughs (user_id,presentation_on DESC NULLS LAST,id DESC)
    WHERE review_visibility='public' AND review IS NOT NULL;
CREATE INDEX public_ratings_owner_game_date_idx ON playthroughs (user_id,game_id,presentation_on DESC NULLS LAST,id DESC)
    WHERE rating_visibility='public' AND rating IS NOT NULL;
CREATE INDEX public_playthroughs_owner_date_idx ON playthroughs (user_id,presentation_on DESC NULLS LAST,id DESC)
    WHERE summary_visibility='public';
CREATE INDEX public_notes_owner_date_idx ON diary_notes (user_id,presentation_on DESC NULLS LAST,id DESC)
    WHERE visibility='public';
CREATE INDEX public_library_owner_idx ON library_entries (user_id,game_id) WHERE visibility='public';
