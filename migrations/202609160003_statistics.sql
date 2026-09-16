-- Enabling a profile must never opt existing or future accounts into a summary.
-- The existing profile revision trigger covers changes to this choice.
ALTER TABLE profiles ADD COLUMN statistics_visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (statistics_visibility IN ('private','public'));
