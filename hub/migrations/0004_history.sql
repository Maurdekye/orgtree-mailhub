-- mail hub v2.0.0, schema 4 (Phase 2, slice 3): history kept until deleted
-- (G4).
--
-- Each message has two copies, the sender's and the recipient's (one, for
-- a message to oneself). Deleting removes the caller's copy only; the row
-- goes when both copies are gone, and its files with it.
ALTER TABLE messages
  ADD COLUMN sender_deleted_at    timestamptz,
  ADD COLUMN recipient_deleted_at timestamptz;

-- A deleted copy's change-log entry keeps the message id, so devices that
-- sync later still learn of the deletion after the row itself is gone.
ALTER TABLE mailbox_log ADD COLUMN deleted_id text COLLATE "C";

-- One conversation, newest first, in each direction; and who an address
-- has written to or heard from (a loose index scan over these).
CREATE INDEX messages_pair_out ON messages (from_slug, to_slug, received_at, n);
CREATE INDEX messages_pair_in ON messages (to_slug, from_slug, received_at, n);
-- unread mail per conversation
CREATE INDEX messages_unread ON messages (to_slug, from_slug) WHERE read_at IS NULL AND recipient_deleted_at IS NULL;
