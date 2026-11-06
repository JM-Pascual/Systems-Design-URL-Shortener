-- Lets the Bloom filter's maintenance job find recently minted codes without
-- scanning the table.
CREATE INDEX links_created_at_idx ON links (created_at);

-- Points at the current filter snapshot, whose chunks live in the object store
-- under `bloom/{id}/{iter}`. Replaced in one transaction, so it only ever names
-- a snapshot that finished uploading.
CREATE TABLE bloom_snapshots (
    id        TEXT PRIMARY KEY,
    taken_at  TIMESTAMPTZ NOT NULL,
    iters     BIGINT[] NOT NULL
);
