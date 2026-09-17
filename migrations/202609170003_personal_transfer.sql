-- Import provenance is private, owner-scoped and survives content deletion so
-- retrying a backup cannot resurrect deleted history. No archived text is kept.
CREATE TABLE personal_import_records (
    user_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    source_profile_id UUID NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('profile','library','playthrough','note','list')),
    source_id UUID NOT NULL,
    target_id UUID NOT NULL,
    fingerprint BYTEA NOT NULL CHECK (octet_length(fingerprint)=32),
    PRIMARY KEY (user_id,source_profile_id,kind,source_id)
);
