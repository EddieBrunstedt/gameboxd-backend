-- Global discovery reads every profile's public items at once instead of one
-- owner's, so these partial indexes cover exactly the rows each read selects.
-- Trending: current players, and completed public runs by chosen day.
CREATE INDEX public_playing_game_idx ON library_entries (game_id)
    WHERE status='playing' AND visibility='public';
CREATE INDEX public_completed_date_idx ON playthroughs (presentation_on,game_id)
    WHERE summary_visibility='public' AND state='completed';
-- The Everyone stream: public reviews by chosen day across all authors.
CREATE INDEX public_reviews_date_idx ON playthroughs (presentation_on DESC,id DESC)
    WHERE review_visibility='public' AND review IS NOT NULL;

-- People search matches a handle prefix or a display-name substring. Both need
-- a trigram index for the planner to combine them; a parameterized prefix
-- LIKE cannot use the handle's B-tree. See 202609080001 for the search path.
SELECT set_config('search_path', current_setting('search_path') || ',public', true);
CREATE INDEX profiles_display_name_trgm_idx ON profiles USING GIN (lower(display_name) gin_trgm_ops);
CREATE INDEX profiles_handle_trgm_idx ON profiles USING GIN (handle gin_trgm_ops);
