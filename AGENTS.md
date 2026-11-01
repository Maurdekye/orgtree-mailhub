# Mail hub development invariants

- 2026-11-01: Translation (feature `translate`, 2.4.0, schema 13). Planned in
  the send transaction (`translate::plan`, after the row, before the push
  wake and the change logs); the provider is called only by the worker
  (`translate::run`, SKIP LOCKED leases, no DB connection held over HTTP).
  `translation` goes into an envelope ONLY when the caller is the message's
  `to_slug` (sync and history); never for the sender, never in the roster
  or directory. A person's `identities.language` IS on their roster entry
  (user 2026-11-01 16:13Z), and changing it bumps roster_seq. Only a
  person's mail is translated, unless the runtime file's
  `translation.translate_agents` is true (user 16:12Z); a sender without a
  language writes in `default_language`; a reader without one gets none.
  Every mention must survive (`translate::keeps_mentions`, the clients'
  grammar ported; user 16:19Z/16:21Z: the model reads the message
  verbatim). Never log or format the key, a provider error or a
  message body. Tests: `--features translate-test` (fake provider on
  loopback; release refuses it). [decided: user 2026-11-01 12:15Z answers +
  12:16Z "Build it" via Orgtree docket
  mail-hub-hubchat-automatic-translation-between-u; verified:
  hub/src/translate.rs, api/mail.rs, api/sync.rs, api/history.rs,
  migrations/0013_translation.sql; tests/translation.rs]

- 2026-10-31: Busy (feature `busy`, 2.3.0) is a part of online. An address
  says it (`POST /api/presence`, memory only, cleared when `now_sets` sees
  the address offline), or a person (`identities.kind`) is busy once the
  latest in-use time of their devices is `BUSY_AFTER` old (one hour, in
  presence.rs, the one constant). In-use times are kept per device in the
  presence slot; `POST /api/devices/active` `false` stores the moment it
  stopped in `devices.active_until` (in the past, so every reader still sees
  "not in use"), read back at start by `seed_presence` from
  `server::prepare`; no device seen: counted from the slot's birth. Sync
  answers send `busy` with every `online` (one print for both in the
  cursor); roster rows carry a `busy` flag. No schema change. Tests move the
  presence clock with feature `test-clock` (release builds refuse it).
  [decided: user 2026-10-30 23:03-23:19Z via Orgtree docket
  org-presence-busy-orange-when-no-org-inbox-holde (an hour, 23:06Z; widened
  to 2.3.0, 23:19Z); verified: hub/src/presence.rs, api/sync.rs, api/mail.rs,
  server.rs; tests/busy.rs; docs/v2-additions.md]

- 2026-10-31: Do not disturb (feature `dnd`, 2.3.0) is a part of busy. An
  address says it beside busy (`POST /api/presence {busy, dnd}`; `dnd` left
  out = false, so an engine from before DND is never DND), memory only.
  `now_sets` returns online, busy and dnd: a DND address online is in `busy`
  too, so a client that knows busy only shows it busy. It ends as busy does,
  when `now_sets` sees the address offline: offline wins over DND, and `dnd`
  lists online addresses only. Sync answers send `dnd` with every `online`
  (in the same cursor print); roster rows carry `dnd`. [decided: user
  2026-10-31 08:51Z via Orgtree docket
  org-presence-busy-orange-when-no-org-inbox-holde; offline wins, user
  09:03Z "Offline for both"; verified: hub/src/presence.rs, api/sync.rs,
  api/mail.rs; tests/busy.rs; docs/v2-additions.md]

- 2026-10-31: Invisible (feature `invisible`, 2.3.0, schema 12): a person's
  status `invisible` is offline to everyone else. `Slot::shown_at` (online
  and not Invisible) feeds every public read: `online()`, `busy()`,
  `dnd()` and `now_sets`; `online_at` stays the truth underneath for their
  own syncs and the declared-flag reset. Roster SQL shows `last_seen` as
  `shown_seen!()` (status_since while Invisible); the idle sweep reads the
  real `last_seen`. Never expose `status` of another address. [decided: user
  2026-10-31 14:13Z, people only 14:14Z; schema 12 approved by the
  coordinator 2026-10-31 via Orgtree docket hubchat-status-picker-add-invisible;
  verified: hub/src/presence.rs, api/mail.rs, api/sync.rs,
  migrations/0012_invisible.sql; tests/busy.rs]

- 2026-10-31: A person's status (feature `status`, 2.3.0, schema 11): a
  person chooses auto, online, busy or dnd (`POST /api/status`; 403 for
  anyone but a person). Kept in `identities.status` and the slot's `status`
  (`seed_presence` reads it back), never cleared by going offline; it shows
  only while they are online (offline wins). `Slot::chosen` feeds
  `busy_if_online` (Online overrides the hour's rule, Busy and DND count as
  said) and `dnd_if_online`. Every sync answer carries `status`, and it is
  folded into the cursor's presence print, so a change wakes their parked
  syncs (`wake_sync`) and a device in use answers at once. A database at
  schema 11 is refused by a hub without it. [decided: user 2026-10-31
  08:52Z (a person sets their status), 09:03Z "Offline for both"; API and
  schema approved by the coordinator 2026-10-31, via Orgtree docket
  org-presence-busy-orange-when-no-org-inbox-holde; verified:
  hub/src/presence.rs, api/sync.rs, migrations/0011_status.sql;
  tests/busy.rs; docs/v2-additions.md]

- 2026-10-14: A refused send is diagnosable from the hub's own log: a 4xx
  answer to `POST /api/send` appends the refusal's `detail` (the same text
  the sender is told) and the recipient slug (`to`) to that request's one
  log line; other 4xx endpoints log their detail the same way. The response
  contract is unchanged, and bodies, attachment contents and credentials
  are never logged. [verified: hub/src/api/mod.rs, api/mail.rs, log.rs;
  tests/send_refusal_log.rs; Orgtree docket
  mail-hub-logs-the-reason-and-recipient-when-it-r]

- 2026-10-13: Typing (feature `typing`) is heartbeats, memory only
  (`crate::typing`, by recipient): never stored, never mail, kept 2 s,
  relayed only while under 1 s old, each told to a sync once (`typing_seq`,
  counting from the start time in microseconds). No start, no stop: nothing
  wakes a sync when heartbeats end, because the receiver drops the indicator
  1 s after the last heartbeat it got. Only a device in use that asked is
  woken, and a wake that brings only heartbeats is answered from memory
  (`typing_only`, no database). `POST /api/typing` takes `slug` and `to`
  only; 10 recipients, 32 per sender within 2 s, one per 300 ms per pair.
  [decided: user 2026-10-13 22:34Z (heartbeats), Orgtree docket
  show-agent-typing-in-hubchat-using-ordered-messa; verified:
  hub/src/typing.rs, api/typing.rs, api/sync.rs, presence.rs;
  tests/typing.rs, tests/typing_load.rs; docs/v2-additions.md]

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
