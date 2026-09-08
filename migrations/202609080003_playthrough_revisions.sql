-- Reviews are long-form text, so a stale editor must be rejected rather than
-- allowed to overwrite newer work. Each playthrough therefore carries a
-- revision counter that clients echo back as an `If-Match` entity tag.
ALTER TABLE playthroughs ADD COLUMN version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0);

-- The counter is maintained by the database rather than by each write, so no
-- future statement can forget it and two updates in one transaction still
-- differ, which `updated_at` alone cannot guarantee.
CREATE FUNCTION playthrough_revision() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.version := OLD.version + 1;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

-- Only a real change is a revision: re-saving an unedited form leaves the row,
-- its timestamp and its tag alone instead of invalidating an entity tag another
-- tab is still legitimately holding.
CREATE TRIGGER playthroughs_revision BEFORE UPDATE ON playthroughs
FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*) EXECUTE FUNCTION playthrough_revision();
