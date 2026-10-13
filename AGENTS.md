# Mail hub development invariants

- 2026-10-13: Local bundled `2.2.0-alpha.0` preserves approved picture/message
  metadata source `33bb02d` and released recovery provenance `2c22e3d`; recovery
  runtime was already common at `7290a59`. Version changes touch only the package
  declaration/lock row; runtime/tests/migrations stay byte-identical to the
  approved source. First start migrates schema 9 to 10, so a 2.1.x hub then
  refuses that database. No automatic backup or downgrade is added. Local
  composition is not publication, packaging or migration acceptance.
  [decided: lead-astra 2026-10-13 14:24UTC; verified: hub/Cargo.toml,
  Cargo.lock, hub/src/db.rs; docs/PROVENANCE.md and OPERATIONS.md]

- 2026-10-12: Message `meta` (feature `meta`, schema 10) is an optional JSON
  object a client attaches on `/api/send`: at most 4096 bytes as compact JSON,
  no NUL, stored with the message and returned unchanged in every envelope
  (poll, sync, history, operator view) only when set; a repeated send keeps the
  first. The hub never reads it and vouches for nothing in it (Hubchat's
  question cards and multi-replies; clients check it themselves). Every
  envelope reader selects it through `ENVELOPE_COLS`.
  [decided: user via lead-astra 2026-10-11 22:40Z (additive bounded metadata);
  verified: api/mail.rs, api/ops.rs, migrations/0010_message_meta.sql;
  tests/message_meta.rs; docs/v2-additions.md]

- 2026-10-12: Roster and picture envelopes carry a domain-separated opaque
  picture_owner token from address/full fingerprint, including absent pictures.
  Never expose the raw authentication fingerprint; different owners of a reused
  address have independent cache/version provenance. [verified: api/pictures.rs,
  mail.rs, sync.rs; docs/profile-pictures.md]

- 2026-10-12: Identity pictures use authenticated bounded canonical PNG records
  for person/org only; chat identities cannot publish. Merge logical revisions
  under the roster lock; automatic reconnect must reuse its revision. Compact
  removal tombstones survive unregister and are scoped to the full fingerprint.
  Image content is never traced. Existing hub_meta storage avoids the reserved
  question-card schema10. [verified: hub/src/api/pictures.rs, mail.rs, sync.rs;
  contract and verification limits: docs/profile-pictures.md]
