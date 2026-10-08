-- mail hub v2.0.0, schema 6 (Phase 2, slice 6): resumable uploads, and the
-- relay that links a new device through a hub.
--
-- An upload in progress: its bytes so far are a file under
-- <HUB_DATA>/uploads/<id>.part (not blobs/, whose partial files a restart
-- clears), `received` of them confirmed. Complete, it becomes an ordinary
-- attachment with the same id and this row goes. One untouched for a day
-- is swept.
CREATE TABLE uploads (
  id         text COLLATE "C" PRIMARY KEY,
  owner_slug text COLLATE "C" NOT NULL,
  name       text NOT NULL,
  total      bigint NOT NULL,
  received   bigint NOT NULL DEFAULT 0,
  sha256     text,
  created_at timestamptz NOT NULL,
  updated_at timestamptz NOT NULL
);
CREATE INDEX uploads_idle ON uploads (updated_at);

-- A sealed payload a signed-in device leaves for a new device under a
-- one-time code (kept only as its sha256), taken once, gone after ten
-- minutes. The hub never sees what is inside.
CREATE TABLE link_relay (
  code_hash  text COLLATE "C" PRIMARY KEY,
  owner_slug text COLLATE "C" NOT NULL,
  sealed     text NOT NULL,
  created_at timestamptz NOT NULL,
  expires_at timestamptz NOT NULL
);
CREATE INDEX link_relay_expiry ON link_relay (expires_at);
