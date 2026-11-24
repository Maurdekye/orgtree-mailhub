-- mail hub v2.6.0, schema 16: blocking (docs/v2-additions.md, "Blocking";
-- Orgtree docket blocking-contacts-on-the-hub-hubchat-1-4-0-orgtr, user
-- 2026-11-24). An address blocks another: the hub refuses the blocked
-- address's sends to it (and, two-way, its own sends to the blocked one),
-- drops its typing heartbeats, and tells both sides in their sync answers.
-- A block belongs to its owner: the blocker's address AND the fingerprint
-- of the secret it was set with. It counts only while that owner holds the
-- address, so a new owner of a freed address inherits none (registering
-- deletes a previous owner's rows), and the same owner coming back keeps
-- them. A hub without schema 16 refuses a database that has it.

CREATE TABLE blocks (
  blocker     text NOT NULL,
  blocker_fp  text NOT NULL,
  blocked     text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (blocker, blocked),
  CHECK (blocker <> blocked)
);
CREATE INDEX blocks_blocked ON blocks (blocked);
