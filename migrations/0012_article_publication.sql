-- Persist page-extracted publication names independently of the best Miniflux entry.

ALTER TABLE articles ADD COLUMN publication TEXT;
