-- Collections describe curation, never ownership or play history. No existing
-- content is copied or published. Favorites use the same ordering/permissions.
CREATE TABLE game_lists (
    id UUID PRIMARY KEY,
    user_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('custom','favorites')),
    title TEXT NOT NULL CHECK (char_length(title) BETWEEN 1 AND 120 AND title ~ '[^[:space:]]'),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description) <= 5000),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','public')),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX game_lists_one_favorites ON game_lists(user_id) WHERE kind='favorites';
CREATE INDEX game_lists_owner_title ON game_lists(user_id,lower(title),id);

CREATE TABLE game_list_members (
    list_id UUID NOT NULL REFERENCES game_lists(id) ON DELETE CASCADE,
    game_id UUID NOT NULL REFERENCES games(id),
    position INTEGER NOT NULL CHECK (position BETWEEN 0 AND 499),
    PRIMARY KEY (list_id,game_id),
    UNIQUE (list_id,position)
);
CREATE INDEX game_list_members_game ON game_list_members(game_id);

CREATE FUNCTION game_list_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.version := OLD.version + 1;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;
CREATE TRIGGER game_lists_revision BEFORE UPDATE ON game_lists
FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION game_list_revision();

CREATE VIEW game_list_details AS
SELECT l.*, (SELECT count(*) FROM game_list_members m WHERE m.list_id=l.id) AS game_count
FROM game_lists l;

-- These projections contain no membership/status/rating from personal history.
CREATE VIEW selected_lists AS
SELECT l.user_id,l.id,l.kind,l.title,l.description,l.game_count,
       p.public_id AS author_id,p.display_name AS author_name
FROM game_list_details l JOIN profiles p ON p.user_id=l.user_id
WHERE l.visibility='public';
CREATE VIEW public_lists AS
SELECT s.* FROM selected_lists s
JOIN profiles p ON p.user_id=s.user_id AND p.public_enabled;
