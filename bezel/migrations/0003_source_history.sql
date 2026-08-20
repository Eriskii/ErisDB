-- Attribution and history.
--
-- Every write carries a server-stamped source: {addr, user, client} —
-- addr observed from the connection (peer IP or iroh endpoint id), user
-- signed into the capability token, client self-described via the
-- X-Bezel-Client header.
--
-- items.source names the last writer (paralleling updated_at). Every
-- changes row snapshots the body the change produced (NULL for deletes)
-- and the source that produced it: the feed is a full, append-only
-- audit log, and any past state can be rolled forward again.

ALTER TABLE items ADD COLUMN source JSONB;

ALTER TABLE changes ADD COLUMN body JSONB;
ALTER TABLE changes ADD COLUMN source JSONB;
-- The revision the change produced, so the feed alone is enough to keep
-- a client's cache true (no per-item refetch). NULL for deletes and ticks.
ALTER TABLE changes ADD COLUMN revision BIGINT;

CREATE INDEX changes_item_seq_idx ON changes (item_id, seq);
