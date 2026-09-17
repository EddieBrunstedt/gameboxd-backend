-- Relationships are private, with no public follower counts or follow chronology.
-- A follower need not publish (or even initialize) a profile.
CREATE TABLE follows (
    follower_id BYTEA NOT NULL REFERENCES gameboxd_auth.users(id) ON DELETE CASCADE,
    followed_id BYTEA NOT NULL REFERENCES profiles(user_id) ON DELETE CASCADE,
    PRIMARY KEY (follower_id, followed_id),
    CHECK (follower_id <> followed_id)
);
CREATE INDEX follows_target_idx ON follows(followed_id);
