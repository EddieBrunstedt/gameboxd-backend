-- Public identity is random and independent of authentication identity. Existing
-- biographies stay private, even when the profile was already enabled.
ALTER TABLE profiles
    ADD COLUMN public_id UUID NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    ADD COLUMN handle TEXT UNIQUE CHECK (handle ~ '^[a-z][a-z0-9_]{2,29}$'),
    ADD COLUMN bio_visibility TEXT NOT NULL DEFAULT 'private' CHECK (bio_visibility IN ('private','public'));

-- Retired names cannot be taken over. Deletion removes the identity reference,
-- but leaves the name unavailable; no public alias/history endpoint exists.
CREATE TABLE profile_handle_reservations (
    handle TEXT PRIMARY KEY CHECK (handle ~ '^[a-z][a-z0-9_]{2,29}$'),
    profile_id UUID REFERENCES profiles(public_id) ON DELETE SET NULL
);
CREATE INDEX profile_handle_reservations_owner_idx ON profile_handle_reservations(profile_id);

-- selected_* applies item permissions only. Its sole ungated reader is the
-- authenticated owner's preview. Public reads must also authorize the profile.
CREATE VIEW selected_reviews AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.review,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.review_visibility='public' AND p.review IS NOT NULL;

CREATE VIEW selected_ratings AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.rating,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.rating_visibility='public' AND p.rating IS NOT NULL;

CREATE VIEW selected_playthroughs AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.state,p.started_on,p.finished_on,p.platform,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.summary_visibility='public';

CREATE VIEW selected_notes AS
SELECT n.user_id,p.game_id,n.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       n.presentation_on,n.text,pr.public_id AS author_id
FROM diary_notes n JOIN profiles pr ON pr.user_id=n.user_id
JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id
JOIN games g ON g.id=p.game_id
WHERE n.visibility='public';

CREATE VIEW selected_library_entries AS
SELECT e.user_id,e.game_id,e.public_id AS id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,e.status,
       latest.rating,latest.id AS rating_id,pr.public_id AS author_id
FROM library_entries e JOIN profiles pr ON pr.user_id=e.user_id
JOIN games g ON g.id=e.game_id
LEFT JOIN LATERAL (
    SELECT r.id,r.rating FROM selected_ratings r
    WHERE r.user_id=e.user_id AND r.game_id=e.game_id
    ORDER BY r.presentation_on DESC NULLS LAST,r.id DESC LIMIT 1
) latest ON true
WHERE e.visibility='public';


-- Gated wrappers preserve the same projections for stable direct item URLs.
CREATE OR REPLACE VIEW public_reviews AS
SELECT s.* FROM selected_reviews s
JOIN profiles pr ON pr.user_id=s.user_id AND pr.public_enabled;

CREATE OR REPLACE VIEW public_ratings AS
SELECT s.* FROM selected_ratings s
JOIN profiles pr ON pr.user_id=s.user_id AND pr.public_enabled;

CREATE OR REPLACE VIEW public_playthroughs AS
SELECT s.* FROM selected_playthroughs s
JOIN profiles pr ON pr.user_id=s.user_id AND pr.public_enabled;

CREATE OR REPLACE VIEW public_notes AS
SELECT s.* FROM selected_notes s
JOIN profiles pr ON pr.user_id=s.user_id AND pr.public_enabled;

CREATE OR REPLACE VIEW public_library_entries AS
SELECT s.* FROM selected_library_entries s
JOIN profiles pr ON pr.user_id=s.user_id AND pr.public_enabled;

