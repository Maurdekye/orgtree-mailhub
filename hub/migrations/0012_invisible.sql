-- mail hub v2.3.0, schema 12: Invisible (docs/v2-additions.md, "A person's
-- status"): a person connected but shown offline to everyone else. A new
-- status word, and when they went Invisible: their roster rows show that as
-- `last_seen` while they stay Invisible, so it does not move on with their
-- calls (the idle sweep keeps the real `last_seen`). NULL unless Invisible.
-- A hub without schema 12 refuses a database that has it ("newer than this
-- hub understands"): going back means restoring a backup from before
-- (docs/OPERATIONS.md).

ALTER TABLE identities DROP CONSTRAINT identities_status_check;
ALTER TABLE identities ADD CONSTRAINT identities_status_check
  CHECK (status IN ('auto', 'online', 'busy', 'dnd', 'invisible'));
ALTER TABLE identities ADD COLUMN status_since timestamptz;
