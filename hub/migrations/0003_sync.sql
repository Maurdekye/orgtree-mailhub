-- mail hub v2.0.0, schema 3 (Phase 2, slice 2): every device gets
-- everything (G1).
--
-- Each address has a change log: one entry per message it sent or received,
-- moved to the end whenever that message changes (a receipt), so a device
-- syncing from its cursor sees every change in hub order, once. `seq` is
-- assigned under the address's mailbox_heads row lock, held until commit,
-- so per address commit order is seq order: a reader that has seen seq N
-- can never later meet a smaller seq committed by a slower writer.
CREATE TABLE mailbox_heads (
  slug text COLLATE "C" PRIMARY KEY,
  seq  bigint NOT NULL
);

CREATE TABLE mailbox_log (
  slug      text COLLATE "C" NOT NULL,
  seq       bigint NOT NULL,
  message_n bigint NOT NULL,          -- messages.n (no foreign key: see sweep)
  PRIMARY KEY (slug, seq),
  UNIQUE (slug, message_n)
);
CREATE INDEX mailbox_log_message ON mailbox_log (message_n);

-- The roster's changes, for directories that sync: a join or an edit
-- stamps the row with the next roster_seq, a leave leaves a tombstone.
-- Writers take an advisory transaction lock before drawing a number, so
-- commit order is seq order here too (roster changes are rare; unchanged
-- re-registrations take no lock).
CREATE SEQUENCE roster_seq;
ALTER TABLE identities ADD COLUMN roster_seq bigint NOT NULL DEFAULT nextval('roster_seq');
CREATE INDEX identities_roster ON identities (roster_seq);

CREATE TABLE roster_gone (
  seq  bigint PRIMARY KEY,
  slug text COLLATE "C" NOT NULL,
  at   timestamptz NOT NULL
);

-- The devices an address syncs from: named by the client (device_id), seen
-- at each sync.
CREATE TABLE devices (
  slug       text COLLATE "C" NOT NULL,
  device_id  text COLLATE "C" NOT NULL,
  name       text NOT NULL DEFAULT '',
  created_at timestamptz NOT NULL,
  last_seen  timestamptz NOT NULL,
  PRIMARY KEY (slug, device_id)
);
