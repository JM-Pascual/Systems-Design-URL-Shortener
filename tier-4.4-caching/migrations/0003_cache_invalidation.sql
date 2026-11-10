-- The tables the invalidator's replication slot streams from the write-ahead log.
CREATE PUBLICATION links_pub FOR TABLE links;
