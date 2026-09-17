-- Private safety state. Existing owner publication choices are untouched.
CREATE TABLE account_blocks (
    blocker_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    blocked_id BYTEA NOT NULL REFERENCES profiles(user_id) ON DELETE CASCADE,
    PRIMARY KEY(blocker_id,blocked_id), CHECK(blocker_id<>blocked_id)
);
CREATE INDEX account_blocks_target ON account_blocks(blocked_id);

-- Provision/revoke through trusted database administration, never an app route.
CREATE TABLE moderation_operators (
    user_id BYTEA PRIMARY KEY REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE
);
CREATE TABLE moderation_restrictions (
    kind TEXT NOT NULL CHECK(kind IN ('profile','review','note','list')),
    target_id UUID NOT NULL,
    owner_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    active BOOLEAN NOT NULL DEFAULT false,
    version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0),
    PRIMARY KEY(kind,target_id)
);
CREATE INDEX moderation_restrictions_owner ON moderation_restrictions(owner_id);
CREATE TABLE moderation_reports (
    id UUID PRIMARY KEY,
    reporter_id BYTEA REFERENCES gameboxd_auth.users(id) ON DELETE SET NULL,
    owner_id BYTEA REFERENCES gameboxd_auth.users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK(kind IN ('profile','review','note','list')),
    target_id UUID NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('spam','harassment','hate','sexual','violence','other')),
    details TEXT NOT NULL CHECK(char_length(details)<=2000),
    evidence JSONB,
    status TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open','resolved','dismissed')),
    version INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(reporter_id,kind,target_id)
);
CREATE INDEX moderation_reports_queue ON moderation_reports(created_at,id);
CREATE INDEX moderation_reports_owner ON moderation_reports(owner_id);
CREATE INDEX moderation_reports_rate ON moderation_reports(reporter_id,created_at);
CREATE TABLE moderation_audit (
    id UUID PRIMARY KEY,
    operator_id BYTEA REFERENCES gameboxd_auth.users(id) ON DELETE SET NULL,
    owner_id BYTEA REFERENCES gameboxd_auth.users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    target_id UUID NOT NULL,
    action TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(char_length(reason) BETWEEN 1 AND 2000),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX moderation_audit_owner ON moderation_audit(owner_id);
CREATE INDEX moderation_audit_order ON moderation_audit(created_at,id);

CREATE FUNCTION moderation_hidden(k TEXT, target UUID) RETURNS BOOLEAN
LANGUAGE sql STABLE SET search_path=gameboxd AS $$
 SELECT EXISTS(SELECT 1 FROM moderation_restrictions WHERE kind=k AND target_id=target AND active)
$$;
CREATE FUNCTION social_allowed(a BYTEA,b BYTEA) RETURNS BOOLEAN
LANGUAGE sql STABLE SET search_path=gameboxd AS $$
 SELECT NOT EXISTS(SELECT 1 FROM account_blocks WHERE (blocker_id=a AND blocked_id=b) OR (blocker_id=b AND blocked_id=a))
 AND NOT EXISTS(SELECT 1 FROM moderation_restrictions WHERE kind='profile' AND active AND owner_id IN (a,b))
$$;

-- Account deletion scrubs quoted evidence and free text, while leaving a minimal
-- private case/action record. Item deletion retains evidence for review, at most
-- 90 days. Operators must not copy evidence into logs or external messages.
CREATE FUNCTION moderation_delete_evidence() RETURNS trigger LANGUAGE plpgsql SET search_path=gameboxd AS $$
BEGIN
 UPDATE moderation_reports SET evidence=NULL,details='' WHERE owner_id=OLD.id;
 UPDATE moderation_audit SET reason='Account deleted' WHERE owner_id=OLD.id;
 RETURN OLD;
END;
$$;
CREATE TRIGGER gameboxd_moderation_delete BEFORE DELETE ON gameboxd_auth.users
FOR EACH ROW EXECUTE FUNCTION moderation_delete_evidence();

-- Visibility writes never lift a restriction; private edits/deletion remain usable.
CREATE FUNCTION moderation_publication_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=gameboxd AS $$
DECLARE profile_id UUID; target UUID; k TEXT; blocked BOOLEAN;
BEGIN
 SELECT public_id INTO profile_id FROM profiles WHERE user_id=NEW.user_id;
 blocked := moderation_hidden('profile',profile_id);
 IF TG_TABLE_NAME='profiles' THEN
   IF NOT NEW.public_enabled THEN RETURN NEW; END IF;
 ELSIF TG_TABLE_NAME='playthroughs' THEN
   IF NEW.summary_visibility<>'public' AND NEW.rating_visibility<>'public' AND NEW.review_visibility<>'public' THEN RETURN NEW; END IF;
   blocked := blocked OR (NEW.review_visibility='public' AND moderation_hidden('review',NEW.id));
 ELSE
   IF NEW.visibility<>'public' THEN RETURN NEW; END IF;
   IF TG_TABLE_NAME='diary_notes' THEN k := 'note'; ELSE k := 'list'; END IF;
   IF TG_TABLE_NAME<>'library_entries' THEN blocked := blocked OR moderation_hidden(k,NEW.id); END IF;
 END IF;
 IF blocked THEN RAISE EXCEPTION 'Public participation is restricted' USING ERRCODE='23514',CONSTRAINT='moderation_restricted'; END IF;
 RETURN NEW;
END;
$$;
CREATE TRIGGER moderation_publication BEFORE UPDATE OF public_enabled,bio_visibility,statistics_visibility ON profiles
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication BEFORE UPDATE OF summary_visibility,rating_visibility,review_visibility ON playthroughs
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication BEFORE UPDATE OF visibility ON diary_notes
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication BEFORE UPDATE OF visibility ON library_entries
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication BEFORE UPDATE OF visibility ON game_lists
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();

CREATE OR REPLACE VIEW selected_reviews AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.review,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.review_visibility='public' AND p.review IS NOT NULL AND NOT moderation_hidden('profile',pr.public_id) AND NOT moderation_hidden('review',p.id);

CREATE OR REPLACE VIEW selected_ratings AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.rating,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.rating_visibility='public' AND p.rating IS NOT NULL AND NOT moderation_hidden('profile',pr.public_id);

CREATE OR REPLACE VIEW selected_playthroughs AS
SELECT p.user_id,p.game_id,p.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       p.presentation_on,p.state,p.started_on,p.finished_on,p.platform,pr.public_id AS author_id
FROM playthroughs p JOIN profiles pr ON pr.user_id=p.user_id
JOIN games g ON g.id=p.game_id
WHERE p.summary_visibility='public' AND NOT moderation_hidden('profile',pr.public_id);

CREATE OR REPLACE VIEW selected_notes AS
SELECT n.user_id,p.game_id,n.id,pr.display_name AS author_name,
       g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url,
       n.presentation_on,n.text,pr.public_id AS author_id
FROM diary_notes n JOIN profiles pr ON pr.user_id=n.user_id
JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id
JOIN games g ON g.id=p.game_id
WHERE n.visibility='public' AND NOT moderation_hidden('profile',pr.public_id) AND NOT moderation_hidden('note',n.id);

CREATE OR REPLACE VIEW selected_library_entries AS
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
WHERE e.visibility='public' AND NOT moderation_hidden('profile',pr.public_id);


CREATE OR REPLACE VIEW selected_lists AS
SELECT l.user_id,l.id,l.kind,l.title,l.description,l.game_count,
       p.public_id AS author_id,p.display_name AS author_name
FROM game_list_details l JOIN profiles p ON p.user_id=l.user_id
WHERE l.visibility='public' AND NOT moderation_hidden('profile',p.public_id)
AND NOT moderation_hidden('list',l.id);

CREATE FUNCTION public_view_allowed(viewer BYTEA, owner BYTEA) RETURNS BOOLEAN
LANGUAGE sql STABLE SET search_path=gameboxd AS $$
 SELECT NOT EXISTS(SELECT 1 FROM account_blocks WHERE (blocker_id=viewer AND blocked_id=owner) OR (blocker_id=owner AND blocked_id=viewer))
$$;
