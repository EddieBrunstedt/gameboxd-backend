-- Substring search on games.name (`?q=`) is unanchored, so the B-tree
-- games_name_idx can order the results but never satisfy the predicate. A
-- trigram GIN index can, once the predicate is LIKE rather than strpos.

-- On a fresh database this installs pg_trgm into the first schema on the
-- search path, which is this app's own. A shared database may already have it
-- elsewhere, commonly public, in which case this is a no-op.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- So append public for the rest of the migration and let gin_trgm_ops resolve
-- either way. The third argument scopes the change to this transaction.
SELECT set_config('search_path', current_setting('search_path') || ',public', true);

-- Not CONCURRENTLY: that cannot run inside a transaction, and the table is
-- small enough that the build is instant. Revisit if games ever grows large
-- enough for the write lock to matter.
CREATE INDEX games_name_trgm_idx ON games USING GIN (lower(name) gin_trgm_ops);
