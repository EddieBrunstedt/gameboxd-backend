-- Owner-chosen sharing defaults, one per publishable component. New content
-- takes the owner's default for its kind when it is created; nothing inherits
-- publication from a parent. Every existing and new account starts private,
-- so this migration publishes nothing.
ALTER TABLE profiles
    ADD COLUMN default_library_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_library_visibility IN ('private','public')),
    ADD COLUMN default_summary_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_summary_visibility IN ('private','public')),
    ADD COLUMN default_rating_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_rating_visibility IN ('private','public')),
    ADD COLUMN default_review_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_review_visibility IN ('private','public')),
    ADD COLUMN default_note_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_note_visibility IN ('private','public')),
    ADD COLUMN default_list_visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (default_list_visibility IN ('private','public'));

-- Defaults make it possible to create content already public, so the moderation
-- guard now checks inserts as well as visibility updates. It refuses only what
-- would *become* public: a write that keeps an existing public choice lifts no
-- restriction, and refusing it would stop a restricted owner from editing their
-- own diary. (Restricted owners' defaults are not applied at all; see
-- SharingDefaults::of.) For inserts OLD is NULL, so every public value is new.
CREATE OR REPLACE FUNCTION moderation_publication_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=gameboxd AS $$
DECLARE profile_id UUID; k TEXT; blocked BOOLEAN;
BEGIN
 IF TG_TABLE_NAME='profiles' THEN
   IF NOT NEW.public_enabled THEN RETURN NEW; END IF;
   IF OLD.public_enabled
      AND NOT (NEW.bio_visibility='public' AND OLD.bio_visibility<>'public')
      AND NOT (NEW.statistics_visibility='public' AND OLD.statistics_visibility<>'public')
   THEN RETURN NEW; END IF;
 ELSIF TG_TABLE_NAME='playthroughs' THEN
   IF NOT ((NEW.summary_visibility='public' AND OLD.summary_visibility IS DISTINCT FROM 'public')
        OR (NEW.rating_visibility='public' AND OLD.rating_visibility IS DISTINCT FROM 'public')
        OR (NEW.review_visibility='public' AND OLD.review_visibility IS DISTINCT FROM 'public'))
   THEN RETURN NEW; END IF;
 ELSE
   IF NOT (NEW.visibility='public' AND OLD.visibility IS DISTINCT FROM 'public') THEN RETURN NEW; END IF;
 END IF;
 SELECT public_id INTO profile_id FROM profiles WHERE user_id=NEW.user_id;
 blocked := moderation_hidden('profile',profile_id);
 IF TG_TABLE_NAME='playthroughs' THEN
   blocked := blocked OR (NEW.review_visibility='public' AND moderation_hidden('review',NEW.id));
 ELSIF TG_TABLE_NAME IN ('diary_notes','game_lists') THEN
   IF TG_TABLE_NAME='diary_notes' THEN k := 'note'; ELSE k := 'list'; END IF;
   blocked := blocked OR moderation_hidden(k,NEW.id);
 END IF;
 IF blocked THEN RAISE EXCEPTION 'Public participation is restricted' USING ERRCODE='23514',CONSTRAINT='moderation_restricted'; END IF;
 RETURN NEW;
END;
$$;
CREATE TRIGGER moderation_publication_insert BEFORE INSERT ON playthroughs
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication_insert BEFORE INSERT ON diary_notes
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication_insert BEFORE INSERT ON library_entries
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
CREATE TRIGGER moderation_publication_insert BEFORE INSERT ON game_lists
FOR EACH ROW EXECUTE FUNCTION moderation_publication_guard();
