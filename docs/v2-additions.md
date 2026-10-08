# What v2 adds to the hub protocol

Phase 2 of mail hub v2.0.0 adds what Hubchat needs beside v1's protocol.
`/api/register`, `/api/poll`, `/api/ack`, `/api/send`, `/api/receipts`,
`/api/attachments` and `/api/roster` keep their meaning, and a client that
never uses an addition sees v1's answers — new envelope keys appear only
on messages that carry them. Where an addition is visible to a v1 client it
is listed in [v2.md](v2.md#deliberate-differences-from-v1).

New routes take a JSON object body and refuse anything else with
400 `the request body must be a JSON object` (v1's routes keep v1's
500 for a malformed body). Refusals are `{"detail": "..."}` as in v1.

## The person kind (G2)

`/api/register` accepts `"kind": "person"` beside `"org"` and `"chat"`; any
other value (including `"PERSON"`) is stored as `"org"`, as v1 did. The kind is
fixed at the first registration, as v1 fixed it: registering again with
another kind keeps the first. The roster, `/ui/data` and the hub's page show
it (the page gives a person a neutral border and a `person` tag).

## Profiles (G2)

`POST /api/profile` changes the caller's display name and about line.

```
X-Org-Auth: <slug>:<secret>
{"name": "Ann Example", "about": "on call this week"}
→ 200 {"ok": true, "profile": <the caller's roster entry>}
```

- `name` (at most 48 characters) and `about` (at most 200) are both optional,
  but at least one must be given; an omitted or null field keeps its value.
  The roster's own spellings `org_name` and `blurb` are accepted too (`name`
  and `about` win when both are given). Leading and trailing whitespace is
  removed; NUL characters are removed (PostgreSQL text cannot hold them); an
  empty string clears the field (clients then show the address).
- When the header signs in several addresses, `slug` names the one to change;
  with several and no `slug`: 422 `several addresses signed in: name the one
  to use (slug)`. A `slug` the header does not sign in: 401
  `no valid credentials for that address`. No valid credentials: 401
  `no valid org credentials`.
- 422 refusals: `name must be a string`, `about must be a string`,
  `name is longer than 48 characters`, `about is longer than 200 characters`,
  `nothing to update: give name and/or about`.
- The address, username and kind never change here. The change counts as
  activity (`last_seen`), and every client sees it in the roster of its next
  poll and through sync.
- Registering again still sets the name and about line from the register
  body, as in v1: a client that edits its profile should send the current
  values when it registers.

## Replies (G3)

`/api/send` accepts an optional `reply_to`: the id of the message this one
answers. The hub stores it as given and returns it unchanged in poll (and the
operator view `/ui/messages`), sync, and (from a later part) history. It is
never checked: the message it names may live on another hub or be gone.

- A message sent without `reply_to` (or with `null`) has exactly v1's
  envelope; with one, `"reply_to"` is the envelope's last key.
- A duplicate send (same id) keeps the first payload's `reply_to`, as it keeps
  the rest of the first payload.
- 422 refusals: `reply_to must be a message id (a string)`,
  `reply_to is longer than 4096 characters`, `reply_to contains a NUL
  character`. (v1 ignored the key, so a v1-era client that sent a non-string
  `reply_to` would now be refused; none does.)

## Every device gets everything (G1)

One address may be used from several devices, all equal. v1's queue gives a
message to whichever client acks it first; sync gives every device all of it.

```
POST /api/sync[?wait=25]
X-Org-Auth: <slug>:<secret>
{"device_id": "pixel-7f3a", "device_name": "Ann's Pixel", "cursor": "<from the last answer>", "wait": 25}
→ 200 {
  "name": "<hub name>",
  "cursor": "<opaque: send it with the next sync>",
  "changes": [{"type": "message", "message": {<envelope>, "delivered_at": null, "read_at": null}}, ...],
  "roster": [<roster entries that joined or changed>],
  "roster_removed": ["<addresses that left>"],
  "online": ["<every address online now>"],      (only when it changed since the cursor)
  "more": false,
  "reset": true                                   (only when the cursor was not this hub's; see below)
}
```

- **What a device gets**: every message the address received or sent (from
  any of its devices, and from v1 clients), in the hub's order, with its
  receipts as they stand now. A message comes again whenever it changes (a
  `delivered` or `read` receipt), so a device that syncs after a change sees
  the message once, current. The envelope is poll's (v1 keys, `reply_to`
  when set) plus `delivered_at` and `read_at` (the receipts' times, or null).
  A copy the address deleted arrives as `{"type": "deleted", "id": ...}`
  (G4). Clients should ignore change types they do not know.
- **Receipts**: `POST /api/receipts` is unchanged and counts for the whole
  address: read on one device is read on all (the first `read` wins, as in
  v1), and the sender's devices see it. v1 senders still get receipts by poll.
- **The roster**: the first sync of a device (no cursor) carries the whole
  roster; later answers carry only entries that joined or changed (name,
  about line), and `roster_removed` the addresses that left. `online` lists
  every address online now, whenever that set changed since the cursor;
  clients treat every other address as offline. Presence alone does not end
  a parked sync early, so `online` is at most `wait` seconds old.
- **The cursor** is per device: one device's progress never hides anything
  from another. Without one (or with `null`/`""`) the device starts from the
  beginning: its address's whole history, in pages. A cursor this hub could
  not have written (a database restored from an older backup) starts the
  device over from the beginning with `"reset": true`; the client should then
  rebuild its copy rather than merge.
- **Paging**: at most 500 message changes and 500 roster changes per answer;
  `more: true` means sync again at once.
- **Long poll**: as `/api/poll` — `wait` (body, or the query as for poll;
  default 25, at most 55 seconds) is how long to park when nothing is new.
  New mail, mail sent from another of the address's devices, a receipt and a
  roster change end the park at once. A parked sync counts as online.
- **Custody**: a device's cursor says it holds everything before it, so mail
  to the address that is still in v1's queue below that cursor is handed over
  at the device's next sync, as an ack would (v1 senders see `fetched`; v1
  polls on the same address stop returning it).
- **Devices**: `device_id` is made by the client: 1-64 printable ASCII
  characters without spaces, stable per installation. `device_name` (up to
  64 characters) is optional; when given it replaces the stored name. A
  device is listed from its first sync; an address keeps at most 100 devices
  (a new one replaces the one seen longest ago).
- **Several addresses** in the header: `slug` names the one to sync (422
  `several addresses signed in: name the one to use (slug)` without it).
- **Refusals** (422 unless said): `device_id is required`, `device_id must be
  1 to 64 printable ASCII characters without spaces`, `device_name must be a
  string`, `device_name is longer than 64 characters`, `cursor is not a sync
  cursor from this hub`, `wait must be a number of seconds`; 400 for a body
  that is not a JSON object; 401 without valid credentials.
- `/api/poll` and `/api/ack` are unchanged: a v1 client on its own address
  sees exactly v1.

```
GET /api/devices[?slug=]
→ 200 {"slug": "...", "devices": [{"device_id", "name", "created_at", "last_seen", "online"}, ...]}
```

Oldest first. `online`: synced within the last 90 seconds. Unregistering an
address removes its devices (its mail and change log stay, as v1 kept its
mail: back with the same key, a device continues from its cursor).

## History kept until deleted (G4)

Mail and files stay on the hub until their owners delete them. An operator
who sets `HUB_RETENTION_DAYS` still has older mail and files swept, as v1
did, and that overrides the kept history; `retention_days` is `null` when
nothing ages out. Uploads that no send ever bound go after 7 days.

Each message has two copies, the sender's and the recipient's (one, for a
message to oneself). History, conversations and sync show the caller's
copies; deleting removes the caller's copy only. A message's row, and its
files, go when neither side has a copy any more, so a file stays
downloadable for the recipient after the sender deletes their copy.

```
GET /api/conversations[?slug=]
→ 200 {"slug": "...", "conversations": [{"with": "<address>", "unread": 2, "last": <message>}, ...]}
```

One row per address the caller has mail with, newest first (at most 1000);
`unread` counts mail from that address the caller has not marked `read`.

```
GET /api/history?with=<address>[&before=<cursor>][&limit=50][&slug=]
→ 200 {"slug": "...", "with": "...", "messages": [<message>, ...], "before": "<cursor>" | null}
```

One conversation, newest first: `limit` messages a page (default 50, at
most 200); `before` from an answer fetches the next older page, and is
`null` once the conversation's start is reached. Each message is poll's
envelope (with `reply_to` when set and its `attachments`) plus its receipt
ladder: `received_at` (the hub took it), `fetched_at` (a client of the
recipient took custody, hub time), `delivered_at` and `read_at` (the
recipient's receipts, as it sent them), each `null` until it happens.
Refusals (422): `with is required: the address of the conversation`,
`limit must be a whole number`, `before is not a history cursor from this
hub`.

```
DELETE /api/messages/{id}[?slug=]             → 200 {"deleted": 1}   (0: already deleted)
DELETE /api/conversations/{address}[?slug=]   → 200 {"deleted": N}
```

- Only the caller's copy goes: the other side keeps theirs, in history and
  in sync. A message the caller neither sent nor received is 404 `no such
  message` (as is an unknown id).
- The caller's devices learn of it through sync as `{"type": "deleted",
  "id": "<message id>"}`, even after the row itself is gone; a receipt
  arriving later does not bring a deleted copy back.
- Deleting a conversation deletes it as it stands when the request arrives,
  in steps of 1000 messages (each its own transaction); mail arriving
  meanwhile stays.
- Mail the recipient deletes while it is still in v1's queue counts as
  handed over: v1 polls stop returning it and a v1 sender gets `fetched`.
- After both copies are gone, the same message id may be sent again; a
  device then sees the deletion and then the new message, in that order.

## Long messages arrive whole (G6), one limit per message (G8)

A body is kept whole, never cut, up to the hub's limit.

- **Sending**: `body` in the send's JSON, as before (the request body may be
  up to 32 MiB), or, for a longer body or to avoid escaping it, upload the
  text first like a file (`POST /api/attachments?name=body.txt`) and send
  `{"body_part": "<its id>"}` instead of `body`. A body part must be the
  sender's own upload, not bound to another message, UTF-8 text without NUL.
  Refusals (422): `give body or body_part, not both`, `body_part must be an
  upload id (a string)`, `unknown body_part '<id>'`, `body_part '<id>' already
  bound`, `body_part is not UTF-8 text`, `body_part contains a NUL character`.
- **Reading (v2 routes)**: sync, history and conversations carry a body up to
  64 KiB whole. A longer one comes as its first 20,000 characters with
  `"body_bytes": <the whole body's size>`; fetch the whole body from
  `GET /api/messages/{id}/body[?slug=]` (sender or recipient while their copy
  exists; `text/plain; charset=utf-8`, streamed, with `Range` support so a
  long one can be resumed). 404 `no such message` otherwise.
- **Reading (v1 routes)**: `/api/poll` and the operator page show a body over
  20,000 characters as its first 20,000 characters followed by
  `\n\n[message continues: N bytes — open it in a client that supports long
  messages]` (N: the whole body's size in bytes). Never a silent cut.
- **The limit** (`max_attachment_bytes`, also reported as
  `max_message_bytes`) bounds one message: its body (UTF-8 bytes) and all its
  attachments together, at most 10 files as before. Over it, 413:
  `{"detail": "message exceeds hub limit of <limit> bytes (body and
  attachments come to <total>)", "max_message_bytes": <limit>}`. The limit is
  the one current when the send arrives (it can change live); a retry of a
  message the hub already accepted is answered as a duplicate whatever the
  limit is now. Each upload is still checked against the limit on its own.
- A long body's file is deleted with the message's last copy (G4).

## A complete directory (G9)

Every registered address is listed to every signed-in client — address,
kind, name, about line, online, last seen — with no opt-out, and stays
listed until it unregisters or the operator removes it
(`orgtree-mailhub remove-address SLUG...`). An idle address is no longer
pruned unless the operator sets `HUB_ORG_RETENTION_DAYS` (then, as v1,
except an address still holding queued mail). Joins, edits and leaves reach
syncing clients through sync (G1); `/api/roster` still lists everyone, as
in v1.

```
GET /api/directory[?q=<text>][&after=<cursor>][&limit=100]
→ 200 {"name": "<hub>", "entries": [<roster entry>, ...], "after": "<cursor>" | null}
```

Entries are roster entries, ordered by address, `limit` a page (default 100,
at most 500); `after` from an answer fetches the next page, `null` on the
last. With `q`, only addresses whose address, name, username or about line
contains it (any case; `%` and `_` are plain characters). Any signed-in
address may read it; 422 `limit must be a whole number`.

## Resumable uploads

`POST /api/attachments` still takes a whole file in one request. For a large
file over a fragile connection, an upload can go in pieces and resume:

```
POST /api/uploads {"bytes": <size>, "name": "photo.jpg", "sha256": "<hex, optional>"}   [?slug=]
→ 200 {"id": "<upload id>", "name": "...", "bytes": <size>, "offset": 0, "complete": false, "expires_at": "..."}

PATCH /api/uploads/{id}?offset=<N>      (raw bytes: the file from N on, or the next piece)
→ 200 {"id", "bytes", "offset": <confirmed>, "complete": false}
→ 200 {"id", "name", "bytes", "offset": <size>, "complete": true}      once every byte is in

GET /api/uploads/{id}       → where it stands (also once complete)
DELETE /api/uploads/{id}    → {"cancelled": true}
```

- The upload's owner is the address that opened it (`slug` when the header
  signs in several); nobody else can see, write or cancel it (404).
- `offset` must be where the upload stands; otherwise 409 `this upload is at
  offset N` with `"offset": N`. A piece cut short keeps what arrived (bytes
  are made durable as they come, at least every 8 MiB), so after a dropped
  connection: `GET` the upload and send from its `offset`. One request writes
  to an upload at a time (409 for a second).
- Complete, the upload is an ordinary attachment with the same id: name it in
  a send's `attachments` (or as `body_part`), download it with
  `GET /api/attachments/{id}` (which already resumes with `Range`).
- With `sha256` given, the bytes are checked when the last one arrives; a
  mismatch empties the upload (422, `"offset": 0`).
- The size is checked against the limit when the upload opens (413 with
  `max_attachment_bytes`); a send still checks the whole message (G8). More
  bytes than the size: 400 with the offset reached. A day without a byte and
  the upload is swept. Uploads in progress survive a hub restart.
- Refusals (422): `bytes must be the upload's size: a whole number`, `name
  must be a string`, `sha256 must be 64 hexadecimal characters`, `offset is
  required: where these bytes start, a whole number`.

## Linking a device through a hub

A new device has no key yet, so it cannot sign in. A signed-in device leaves
it a payload sealed by the clients (the identity key, hub list and profile:
the hub never sees inside) under a one-time code the new device shows:

```
POST /api/link/put {"code": "<one-time code>", "sealed": "<encrypted payload>"}   (signed in)
→ 200 {"ok": true, "expires_at": "..."}            409 while the same code still waits
POST /api/link/take {"code": "<one-time code>", "wait": 25}                       (no sign-in)
→ 200 {"sealed": "...", "from": "<address that left it>"}    404 when nothing waits by then
POST /api/link/cancel {"code": "<one-time code>"}                                 (signed in)
→ 200 {"cancelled": true}
```

- A payload waits ten minutes and is taken once. `take` long-polls up to
  `wait` seconds (default 25, at most 55) and answers as soon as it arrives.
- Codes are 6-128 characters and travel only in request bodies, so no request
  log holds one; the hub stores only their sha256. `sealed` is at most 1 MiB.
- Only the address that left a payload can cancel it.

## Per-device keys, and signing a device out (G5)

The shared v1 secret keeps working until the address moves on. With device
keys, every device has its own Ed25519 key, enrolled by the address's
identity key, and signs its calls with it; signing one device out cuts it
off while the others keep working, under the same address (ruling 8
October: the identity key lives on every device; signing a device out also
rotates it).

Keys and signatures are base64url without padding: public keys 32 bytes,
signatures 64 (Ed25519, verified strictly). Texts are signed as UTF-8, lines
joined by `\n`, no trailing newline. The hub never sees a private key.

**1. The identity key** (once, with the shared secret):

```
POST /api/identity {"identity_key": "<public key>"}        X-Org-Auth: <slug>:<shared secret>
GET  /api/identity → {"slug", "identity_key", "key_version": 0, "shared_key": true, "sealed": ...}
```

Setting the same key again is fine; a different one is refused (409) — a new
identity key only comes by rotation.

**2. Enrolling a device** (no other sign-in: the certificate is the proof):

```
POST /api/devices {"slug", "device_id", "public_key": "<the device's key>", "created": "<ISO time>",
                   "signature": "<identity key over the certificate>", "name": "<optional>"}
certificate =
orgtree-hub device v2
address=<slug>
device_id=<device_id>
public_key=<public_key>
created=<created>
```

Refusals: 409 when the address has no identity key, or the device id was
signed out (enrol under a new id), or 100 devices are enrolled; 401 when the
identity key did not sign it; 422 for a malformed field. Enrolling the same
device again replaces its key.

**3. Signed calls**: any route, `X-Org-Auth: <slug>:dev1.<device_id>.<unix ms>.<signature>`,
the device's key over

```
orgtree-hub call v2
<slug>
<device_id>
<unix ms>
```

The time must be within 10 minutes of the hub's (its `Date` header tells the
hub's clock). Like the shared secret it is a bearer credential for those
minutes, so send it over TLS or a trusted network. A device's sync must name
its own `device_id` (422 otherwise).

**4. Signing a device out** (from any signed-in device, or with the shared
secret while it still works):

```
DELETE /api/devices/{device_id}
{"identity_key": "<new identity public key>",
 "signature": "<CURRENT identity key over the rotation statement>",
 "sealed": {"<each remaining enrolled device_id>": "<the new identity secret, sealed to that device>"}}
rotation statement =
orgtree-hub rotate v2
address=<slug>
sign_out=<device_id>
identity_key=<new identity public key>
key_version=<current key_version + 1>
→ 200 {"signed_out": "<device_id>", "rotated": true, "key_version": <n>}
```

- From then on the device's calls are refused (401), its parked sync ends
  with 401, and it cannot enrol again under that id. The other devices keep
  working with their own keys; the address does not change.
- The old identity key no longer enrols or rotates; the shared v1 secret
  stops working (a device that still syncs with it must be enrolled first).
- Every remaining enrolled device must get a sealed copy (422 naming one
  that has none; 422 for a copy addressed to a device not enrolled here);
  how it is sealed is the clients' choice (for example a sealed box to the
  device's key). Each device sees the new `identity_key_version` in its next
  sync answer and collects its copy from `GET /api/identity` (signed by
  itself: `"sealed"` holds its copy).
- 401 when the current identity key did not sign the statement; 404 for an
  unknown device; 409 when it is already signed out.
- An address with no identity key: `DELETE /api/devices/{id}` (no body) only
  takes the device off the list and out of sync (`"rotated": false`).

**5. The shared key off**: `POST /api/identity {"shared_key": false}` turns
the shared v1 secret off for good, once at least one device has its own key
(422 before; `true` is refused).

`GET /api/devices` also shows each device's `public_key` (null for a device
that only syncs with the shared secret) and `signed_out_at`.

## Telling what a hub supports

`/healthz` also reports `"version"` (`"2.0.0"`) and `"features"`, the
additions this hub serves: `person`, `profile`, `reply_to`, `sync`,
`devices`, `history`, `delete`, `long_messages`, `message_limit`,
`directory`, `uploads`, `link`, `device_keys`. A v1 hub reports neither.
