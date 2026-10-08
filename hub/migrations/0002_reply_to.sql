-- mail hub v2.0.0, schema 2 (Phase 2, slice 1): inline replies (G3).
-- A message may name the message it answers. The reference is kept as the
-- sender gave it and never checked: the message it names may live on
-- another hub, or have been deleted.
--
-- The "person" kind (G2) needs no schema change: identities.kind is free
-- text (org | chat | person).

ALTER TABLE messages ADD COLUMN reply_to text;
