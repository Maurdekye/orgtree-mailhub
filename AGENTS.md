# Mail hub development invariants

- 2026-10-12: Identity pictures use authenticated bounded canonical PNG records
  for person/org only; chat identities cannot publish. Merge logical revisions
  under the roster lock; automatic reconnect must reuse its revision. Compact
  removal tombstones survive unregister and are scoped to the full fingerprint.
  Image content is never traced. Existing hub_meta storage avoids the reserved
  question-card schema10. [verified: hub/src/api/pictures.rs, mail.rs, sync.rs;
  contract and verification limits: docs/profile-pictures.md]
