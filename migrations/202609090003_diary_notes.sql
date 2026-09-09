-- Notes belong to a stable experience, never to library membership. The
-- composite key prevents a note's diary owner from differing from its parent.
ALTER TABLE playthroughs ADD CONSTRAINT playthroughs_owner_id_key UNIQUE (user_id,id);

CREATE TABLE diary_notes (
    id UUID PRIMARY KEY,
    user_id BYTEA NOT NULL,
    playthrough_id UUID NOT NULL,
    text TEXT NOT NULL CHECK (char_length(text) BETWEEN 1 AND 10000 AND text ~ '[^[:space:]]'),
    presentation_on DATE,
    version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (user_id,playthrough_id) REFERENCES playthroughs(user_id,id) ON DELETE CASCADE
);

CREATE INDEX diary_notes_owner_date_idx ON diary_notes (user_id,presentation_on DESC NULLS LAST,id DESC);
CREATE INDEX diary_notes_parent_date_idx ON diary_notes (user_id,playthrough_id,presentation_on DESC NULLS LAST,id DESC);

CREATE FUNCTION diary_note_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.version := OLD.version + 1;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER diary_notes_revision BEFORE UPDATE ON diary_notes
FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION diary_note_revision();

-- Owner-only projection. Metadata is locally available even when IGDB is down.
CREATE VIEW diary_note_details AS
SELECT n.*,g.igdb_id,g.name AS game_name,g.slug AS game_slug,g.cover_url
FROM diary_notes n JOIN playthroughs p ON p.id=n.playthrough_id AND p.user_id=n.user_id
JOIN games g ON g.id=p.game_id;
