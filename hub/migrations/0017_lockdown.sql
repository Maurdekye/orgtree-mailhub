-- mail hub v2.6.0, schema 17: lockdown (docs/v2-additions.md, "Lockdown";
-- Orgtree docket org-mailbox-lockdown-allow-list-mode-that-denies, user
-- 2026-11-25 13:26Z and 13:37Z). An address in lockdown takes messages
-- only from the addresses on its list: the hub refuses everyone else's
-- sends to it, drops their typing heartbeats, and shows them a lock in
-- place of its presence. Like a block, a lockdown belongs to its owner: the
-- address AND the fingerprint of the secret it was set with. It counts only
-- while that owner holds the address; registering deletes a previous
-- owner's. A hub without schema 17 refuses a database that has it.

CREATE TABLE lockdowns (
  address   text PRIMARY KEY,
  owner_fp  text NOT NULL,
  since     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE lockdown_allow (
  address  text NOT NULL REFERENCES lockdowns ON DELETE CASCADE,
  allowed  text NOT NULL,
  PRIMARY KEY (address, allowed),
  CHECK (address <> allowed)
);
