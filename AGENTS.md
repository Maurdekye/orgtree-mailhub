# Mail hub development invariants

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
