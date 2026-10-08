-- mail hub v2.0.0, schema 5 (Phase 2, slice 4): long messages arrive whole
-- (G6).
--
-- A body is kept whole, never cut. One longer than 64 KiB lives in a file
-- under blobs/ (an attachments row bound to the message, named here by
-- body_part); messages.body then holds its first 20,000 characters and
-- body_bytes the whole body's size in bytes. Both are NULL for a body kept
-- whole in the row.
ALTER TABLE messages
  ADD COLUMN body_bytes bigint,
  ADD COLUMN body_part  text COLLATE "C";
