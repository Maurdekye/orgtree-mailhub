-- mail hub v2.3.0, schema 11: a person's chosen status (POST /api/status;
-- docs/v2-additions.md, "A person's status"): 'auto' (the hub's own rules),
-- 'online', 'busy' or 'dnd' (Do not disturb). Kept until they choose again;
-- it shows only while they are online. A hub without schema 11 refuses a
-- database that has it ("newer than this hub understands"): going back
-- means restoring a backup from before (docs/OPERATIONS.md).

ALTER TABLE identities ADD COLUMN status text NOT NULL DEFAULT 'auto'
  CHECK (status IN ('auto', 'online', 'busy', 'dnd'));
