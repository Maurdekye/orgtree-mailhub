-- mail hub v2.6.0, schema 15: "last seen" per kind (docs/v2-additions.md,
-- "Last active"; Orgtree docket hubchat-last-seen-time-for-orgs-people-and-indep,
-- user 2026-11-24). When the address was last active, or NULL for never or
-- not known: a person, when one of their devices was last in use (Hubchat in
-- front); an org, when the agents in charge of its org mailbox last finished
-- a turn (Orgtree says it, `POST /api/presence`); an agent session (kind
-- 'chat'), when it last sent a message to anyone. `last_seen` (the last
-- call) is unchanged. A hub without schema 15 refuses a database that has
-- it ("newer than this hub understands"): going back means restoring a
-- backup from before (docs/OPERATIONS.md).

ALTER TABLE identities ADD COLUMN active_at timestamptz;

-- a person: the latest moment one of their devices was in use, never later
-- than now (a device in use holds a lease that ends in the future)
UPDATE identities i SET active_at = d.at
  FROM (SELECT slug, LEAST(max(active_until), now()) AS at FROM devices
         WHERE active_until IS NOT NULL AND revoked_at IS NULL GROUP BY slug) d
 WHERE i.slug = d.slug AND i.kind = 'person';

-- an agent session: its last message sent to anyone
UPDATE identities i SET active_at = m.at
  FROM (SELECT from_slug AS slug, max(received_at) AS at FROM messages GROUP BY from_slug) m
 WHERE i.slug = m.slug AND i.kind = 'chat';
