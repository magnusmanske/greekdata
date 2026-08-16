-- Where an entity's coordinates came from.
--
-- Pharmacy coordinates are published with the duty roster itself. Hospital rotas are
-- name-only, so their coordinates are matched in from elsewhere, and a reader deciding
-- where to drive deserves to know which is which.
ALTER TABLE entity ADD COLUMN location_source TEXT;
