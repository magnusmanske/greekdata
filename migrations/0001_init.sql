-- Entities are the nouns: a pharmacy, a hospital, a cinema, a film.
CREATE TABLE entity (
    id           INTEGER PRIMARY KEY,
    kind         TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    -- Accent-folded, uppercased name, used for matching messy source spellings.
    name_folded  TEXT    NOT NULL,
    address      TEXT,
    municipality TEXT,
    lat          REAL,
    lon          REAL,
    url          TEXT,
    phone        TEXT,
    created_at   TEXT    NOT NULL,
    updated_at   TEXT    NOT NULL
);

CREATE INDEX entity_kind_folded ON entity (kind, name_folded);
CREATE INDEX entity_geo         ON entity (lat, lon);

-- Alternative spellings seen in source documents, so a typo does not create a duplicate.
CREATE TABLE entity_alias (
    entity_id    INTEGER NOT NULL REFERENCES entity (id) ON DELETE CASCADE,
    alias        TEXT    NOT NULL,
    alias_folded TEXT    NOT NULL,
    PRIMARY KEY (entity_id, alias_folded)
) WITHOUT ROWID;

CREATE INDEX entity_alias_folded ON entity_alias (alias_folded);

-- Identifiers assigned elsewhere: wikidata, imdb, or a source's own primary key.
CREATE TABLE entity_external_id (
    entity_id INTEGER NOT NULL REFERENCES entity (id) ON DELETE CASCADE,
    scheme    TEXT    NOT NULL,
    value     TEXT    NOT NULL,
    PRIMARY KEY (scheme, value)
) WITHOUT ROWID;

CREATE INDEX entity_external_id_entity ON entity_external_id (entity_id);

-- One fetched source document. Provenance for everything derived from it.
CREATE TABLE snapshot (
    id             INTEGER PRIMARY KEY,
    source_id      TEXT    NOT NULL,
    url            TEXT    NOT NULL,
    sha256         TEXT    NOT NULL,
    fetched_at     TEXT    NOT NULL,
    -- The date the document is *about*, which is not the date it was published.
    published_date TEXT,
    -- 0 for an original; higher for each corrected reissue of the same date.
    revision       INTEGER NOT NULL DEFAULT 0,
    label          TEXT,
    UNIQUE (source_id, url, sha256)
);

CREATE INDEX snapshot_source_date ON snapshot (source_id, published_date, revision);

-- Something true of an entity on a given day. Superseded rows are kept, never deleted.
CREATE TABLE property (
    id          INTEGER NOT NULL PRIMARY KEY,
    entity_id   INTEGER NOT NULL REFERENCES entity (id),
    snapshot_id INTEGER NOT NULL REFERENCES snapshot (id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,
    on_date     TEXT    NOT NULL,
    starts_at   TEXT,
    ends_at     TEXT,
    payload     TEXT    NOT NULL DEFAULT '{}',
    -- 1 once a later revision of the same source and date replaced this row.
    superseded  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX property_lookup   ON property (kind, on_date, superseded);
CREATE INDEX property_entity   ON property (entity_id, on_date);
CREATE INDEX property_snapshot ON property (snapshot_id);

-- Data problems found while parsing, surfaced by `greekdata report` instead of
-- aborting an ingest or being silently discarded.
CREATE TABLE ingest_issue (
    id          INTEGER PRIMARY KEY,
    snapshot_id INTEGER REFERENCES snapshot (id) ON DELETE CASCADE,
    source_id   TEXT    NOT NULL,
    severity    TEXT    NOT NULL,
    code        TEXT    NOT NULL,
    detail      TEXT    NOT NULL,
    created_at  TEXT    NOT NULL
);

CREATE INDEX ingest_issue_source ON ingest_issue (source_id, severity);
