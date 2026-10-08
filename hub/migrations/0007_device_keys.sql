-- mail hub v2.0.0, schema 7 (Phase 2, slice 7): per-device keys and signing
-- a device out (G5).
--
-- An address may register an identity key (Ed25519, public half only). With
-- it, devices enrol their own keys by a certificate the identity key signs,
-- and sign their calls with them. Signing a device out revokes its key and
-- rotates the identity key: the new key, sealed by the client to each
-- remaining device, waits here for them. The first rotation (or the owner)
-- turns the shared v1 key off. The address never changes.
ALTER TABLE identities
  ADD COLUMN identity_key       text,
  ADD COLUMN key_version        integer NOT NULL DEFAULT 0,
  ADD COLUMN shared_key_enabled boolean NOT NULL DEFAULT true;

ALTER TABLE devices
  ADD COLUMN public_key     text,
  ADD COLUMN cert_created   text,
  ADD COLUMN cert_signature text,
  ADD COLUMN revoked_at     timestamptz;

-- the new identity key, sealed to one remaining device, per rotation
CREATE TABLE identity_key_drops (
  slug        text COLLATE "C" NOT NULL,
  device_id   text COLLATE "C" NOT NULL,
  key_version integer NOT NULL,
  sealed      text NOT NULL,
  created_at  timestamptz NOT NULL,
  PRIMARY KEY (slug, device_id, key_version)
);
