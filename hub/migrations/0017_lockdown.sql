-- mail hub v2.6.0, schema 17: lockdown (docs/v2-additions.md, "Lockdown";
-- Orgtree docket org-mailbox-lockdown-allow-list-mode-that-denies, user
-- 2026-11-25 13:26Z and 13:37Z). An address in lockdown takes messages
-- only from the addresses on its list: the hub refuses everyone else's
-- sends to it, drops their typing heartbeats, and shows them a lock in
-- place of its presence.
--
-- A lockdown belongs to its owner, the address AND the fingerprint of the
-- secret it was set with (`owner_fp`), as a block does: it counts only
-- while that owner holds the address, and registering deletes a previous
-- owner's row. `rev` orders the owner's changes (a lower one is refused);
-- the row stays, `on = false`, when the lockdown is lifted, so an old
-- change arriving late cannot turn it on again.
--
-- An allow entry names an ADDRESS on this hub and follows it: a new key or
-- registration for a listed address stays allowed (user 2026-11-25 14:08Z,
-- answer 7, replacing review B1's identity binding: "its the orgs
-- responsibility to cross their name from the whitelist"). Entries are this
-- hub's own, so the same name on another hub never matches (review B3).
-- A hub without schema 17 refuses a database that has it.

CREATE TABLE lockdowns (
  address   text PRIMARY KEY,
  owner_fp  text NOT NULL,
  on_       boolean NOT NULL,
  rev       bigint NOT NULL DEFAULT 0,
  since     timestamptz
);

CREATE TABLE lockdown_allow (
  address     text NOT NULL REFERENCES lockdowns ON DELETE CASCADE,
  allowed     text NOT NULL,
  PRIMARY KEY (address, allowed),
  CHECK (address <> allowed)
);
CREATE INDEX lockdown_allow_allowed ON lockdown_allow (allowed);
