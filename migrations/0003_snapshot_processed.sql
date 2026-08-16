-- When a snapshot was last parsed into records.
--
-- Documents are identified by the SHA-256 of their bytes, so re-fetching one that has
-- not changed upstream yields the snapshot row we already have. Knowing it was parsed
-- successfully lets a later run skip the work entirely, which matters most for the
-- hospital rotas: those are PDFs, and parsing one costs far more than fetching it.
ALTER TABLE snapshot ADD COLUMN processed_at TEXT;

-- Snapshots already in the database that produced records were plainly parsed, so an
-- upgrade does not re-do every document once.
UPDATE snapshot
SET processed_at = fetched_at
WHERE EXISTS (SELECT 1 FROM property WHERE property.snapshot_id = snapshot.id);
