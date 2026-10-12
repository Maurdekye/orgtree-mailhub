# Identity profile pictures

Implementation contract for the approved 2026-10-12 profile-picture feature.
Owners are `person` and `org` identities. Direct sessions (`chat`) cannot publish
pictures. Pictures displayed inside conversations still belong to the identity.

## Wire and persistence

Capability `profile_pictures` advertises authenticated GET/POST
`/api/profile-picture`. GET takes an address in `slug`; any authenticated hub
identity can read a registered eligible identity's picture. POST takes an owned
`slug` and a `picture` record. Existing shared-key and device-key authentication
apply; the stored identity kind is authoritative. No URL is accepted.

A record contains `revision`, `sha256`, `width`, `height`, `bytes`, and `data`
(standard base64 of canonical PNG). Removal uses the same record with null
`data`/`sha256` and zero sizes. Directory/roster entries expose `picture` metadata
without `data`. Missing metadata means unsupported/never set, not removal.

The revision is a fixed-width, zero-padded 20-digit unsigned logical counter,
a hyphen, and a 32-character lowercase random operation ID. An explicit edit
increments the highest counter the editing device has observed. Automatic
publication, retries, relay across the owner's hubs, and reconnect reuse the
same revision. Lexicographically greatest revision wins; equal revisions with
different content are refused. Concurrent unseen edits converge by operation
ID, not by wall-clock time. A later edit after observing either increases the
counter. A stale cached picture cannot replace a newer removal tombstone.

Current data occupies one bounded `hub_meta` key
`profile_picture:<slug>:<full identity fingerprint>`.
This existing JSON store avoids colliding with paused question schema10.
Updates merge under the existing roster transaction lock and advance its
sequence only on a changed record. Identity departure deletes image records
in the same transaction, but keeps compact removal tombstones. Re-registering
the same identity must not let a stale device resurrect a removed image;
a different full fingerprint cannot inherit its state. A picture is not a message attachment; retention,
mail deletion, transfer custody and per-message byte limits do not apply.

## Images and clients

Editors accept static PNG/JPEG/WebP, at most5MiB and16 megapixels, offer a
preview and square crop, normalize orientation, strip metadata, and output
static PNG at most512x512 and256KiB. Animation and SVG are refused. The hub
accepts only the bounded normalized PNG representation and verifies it before
the write transaction. Byte-bearing values are never trace return values.

Clients keep durable versioned metadata and verified bounded image bytes.
They fetch images through authenticated native HTTP, never arbitrary renderer
URLs. Revision/hash checks prevent a late old response from replacing current
content. Removal invalidates presentation immediately even while another hub
is offline. Identity switching clears the old identity's owned/cache state.
Older hubs are skipped by capability; older clients ignore additive metadata.
Absent/unavailable/unsupported images retain the existing initials/glyph.

## Verification required before integration

Scratch proof must cover eligible and excluded kinds, wrong owner/device auth,
body/decoded dimensions/PNG/hash bounds, atomic metadata and directory sequence,
equal replay/conflict, stale lower revisions, tombstones and concurrent edits,
identity departure/re-registration, unchanged messages/transfers, and old-client
registration not removing pictures. No deployment or live profile is authorized.

## Measured verification (2026-10-12)

The focused `profile_pictures` target passes 2 tests against isolated PostgreSQL:
validation plus HTTP ownership/eligibility/revision/roster/removal/lifetime paths.
The final run also enrolls a device through the real identity/device APIs,
proves signed GET/POST success, refuses another identity's writes and a forged
device signature, and proves shared-key refusal plus continuing device access
after shared-key disable. Compilation took7.03s and tests1.68s, exit0.
This is isolated PostgreSQL and in-process HTTP handler verification. Device
revocation races, installed clients and pixel UI are not measured here. Separate
client/Orgtree receipts cover their paths; independent review remains pending.
