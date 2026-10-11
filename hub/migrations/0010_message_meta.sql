-- mail hub, schema 10: a message's `meta` (docs/v2-additions.md, "Message
-- meta"): a small JSON object its sender writes and every reader gets back
-- unchanged. Hubchat puts question cards and multi-replies in it. The hub
-- never reads it.

ALTER TABLE messages ADD COLUMN meta jsonb;
