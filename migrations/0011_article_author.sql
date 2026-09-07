-- Persist page-extracted authors independently of the best Miniflux entry.

ALTER TABLE articles ADD COLUMN author TEXT;
