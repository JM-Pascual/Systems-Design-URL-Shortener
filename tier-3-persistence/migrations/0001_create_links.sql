CREATE TABLE links (
    code        TEXT PRIMARY KEY,
    long_url    TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ,
    user_id     UUID
);

-- Postgres's own id generator: `nextval('link_ids')` is what `Store::shorten`
-- calls before base62-encoding the result. See the README's discussion
-- question on why this is non-transactional, and why that's fine.
CREATE SEQUENCE link_ids;
