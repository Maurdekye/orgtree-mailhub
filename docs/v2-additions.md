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
another kind keeps the first, with one exception. An address registered as
`"chat"` that registers again (with its own secret) as `"person"` becomes a
person: Hubchat registered its people as chats on v1 hubs, which had no person
kind, and a client that sends `"person"` on every connect fixes itself on its
next one. The change reaches every client like any roster change (poll, sync,
roster). Nothing else changes kind: not person to chat, nothing into or out of
`"org"`. The roster, `/ui/data` and the hub's page show it (the page gives a
person a neutral border and a `person` tag).

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
  "active": ["<this address's other devices in use>"],   (every answer; see "Active devices")
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
  cursor from this hub`, `wait must be a number of seconds`, and the two
  `start` refusals ("Lazy history" below); 400 for a body that is not a JSON
  object; 401 without valid credentials.
- **Starting from now** instead of from the beginning: see "Lazy history"
  below (v2.0.1).
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
hub` (neither a cursor nor a time). Since v2.0.1, `before` may also be a
time ("Lazy history" below).

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
  messages]` (N: the whole body's size in bytes). Never a silent cut. The
  envelope then also carries `"body_bytes": N`, so a v1 client that knows the
  body route can fetch the rest.
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

Once the shared secret stops working (turned off, or the identity key rotated
by signing a device out), registering with it is refused too: `POST
/api/register` answers 401 `this address no longer accepts its shared
secret` and changes nothing. A device never needs `/api/register`
(`/api/register` only ever takes the shared secret: it claims an address by
that secret's fingerprint). Every signed call (a sync, a profile change)
counts as the address being seen, a parked sync keeps it online, and `POST
/api/profile` changes its name and about line.

`GET /api/devices` also shows each device's `public_key` (null for a device
that only syncs with the shared secret) and `signed_out_at`.

## Active devices (notifications follow the device in use)

When a person uses one of their devices, the others should not also pop
notifications. The device in use says so, and every sync answer tells the
other devices who that is.

```
POST /api/devices/active
X-Org-Auth: <slug>:<secret>            (or the device's own signed call)
{"device_id": "pixel-7f3a", "active": true, "slug": "<when the header carries several>"}
→ 200 {"slug": "...", "device_id": "pixel-7f3a", "active": true, "active_until": "<hub time + 90 s>"}
```

- `true` means "this device is in use" for **90 seconds** from now. A
  client sends it when its window gains focus (or its app comes to the
  foreground) and about once a minute while it stays in use. `false` ends
  it at once (blur, idle, lock, background). Nothing sweeps: a device that
  crashes or sleeps stops counting when its 90 seconds run out.
- **Every sync answer** carries `"active"`: the address's other devices in
  use as of that answer, never the asking device and never a signed-out one.
  It is read when the answer is built, so a device deciding whether to
  notify about the mail in an answer sees who was active as of that same
  answer. A change of `active` never ends a parked sync by itself; the next
  answer carries it.
- `GET /api/devices` shows each device's `active` (true or false) and
  `active_until` (when it lapses, or null when the device is not in use).
- The call takes the same credentials, `slug` and `device_id` rules as
  sync: a call signed by a device speaks only for that device (422 `device_id
  must be the signing device's own`), a signed-out device is refused (401
  `this device was signed out`), and a device never seen before is made, as
  at its first sync. Refusals (422): `active is required: true or false`,
  `active must be true or false`, and sync's own `device_id` refusals.
- `/healthz` lists the feature as `active`. A client that does not find it
  (a v1 hub, or an older v2) notifies as before. Clients that never send it
  are unaffected: no device of theirs is ever active.

## Lazy history: a new device starts from now (v2.0.1)

A device that syncs from the beginning first downloads every message its
address ever had, and kept history only grows. A device can instead start
from now and fetch older mail page by page, as its user scrolls back.

```
POST /api/sync
{"device_id": "pixel-7f3a", "start": "now", "wait": 0}       (a device's first sync: no cursor)
→ 200 {..., "changes": [], "roster": [<the whole roster>], "online": [...], "start": "now", "now": 1760000000123}
```

- **The first answer** carries none of the address's mail so far. It
  carries the whole roster (paged by `more`, as any first sync), `online`
  and `active`, and says `"start": "now"` back. A hub without this feature
  ignores `start` and syncs from the beginning; the missing echo says so.
- **After it**, the device's answers carry what happens from then on: new
  mail, mail sent from the address's other devices, and changes to older
  messages (a receipt, a deletion). Apply those to the copies the device
  holds, and store or ignore the rest.
- **Older mail** comes from `GET /api/history`, one page at a time, and the
  chat list from `GET /api/conversations` (each entry has its newest
  message and its unread count).
- **Custody**: a device that started from now holds nothing from before its
  start. Mail still in v1's queue from before then stays there: v1 polls on
  the address still return it, and its v1 senders see no `fetched`. Mail
  after the start is handed over as for any device.
- **Its cursor** stays opaque. It records where the device started, for
  the device's whole life. A cursor this hub did not write starts the device
  over **from now** (not from the beginning), with `"reset": true`.
- **Refusals** (422): `start is only for a device's first sync (send no
  cursor)`, `start must be "now"`.

```
GET /api/history?with=<address>&before=<unix ms>[&limit=50]
```

- A bare whole number in `before` is a time: the page holds what this hub
  received strictly before that millisecond (`received_at < ms`), newest
  first. To include that millisecond, ask for ms + 1. The answer is as
  ever: its `before` is the exact cursor for the page before (null at the
  conversation's start), so a client goes on from there with the cursor.
- The exact cursor (`<ms>-<n>` from an answer) works as before. Anything
  that is neither is refused as before: 422 `before is not a history cursor
  from this hub`.

**The hub's clock**: `/healthz` and every sync answer carry `"now"`, the
hub's time in unix milliseconds when the answer was made. A client on
several hubs can estimate each hub's clock offset from it (and the round
trip), since received times are each hub's own.

`/healthz` lists the feature as `lazy_history`.

## The relay-only door's address (v2.0.1)

When the hub runs its relay-only door (`HUB_PUBLIC`), `/healthz` on the
main port says where the door listens. A client on the same machine
(Hubchat on the PC, linking a phone) can put that address in a setup code
instead of guessing:

```
GET /healthz                                  (on the main port)
→ 200 {..., "door": {"port": 7371, "bind": "100.64.1.2"}}
```

- `port` and `bind` are where the door's listener actually listens. `bind`
  is always an IP address: a host name in `HUB_PUBLIC_BIND` shows as the
  address it resolved to, and port 0 as the port the system picked.
- `"0.0.0.0"` (or `"::"` for IPv6) means every address of the machine, and
  the client picks one the phone can reach. Any other value is the one
  address the door accepts connections on.
- **No door, no field.** A hub that runs no door leaves `door` out. So does
  a hub without this feature (v2.0.0, v1); `features` tells the two apart.
- **The main port only.** The door's own `/healthz` never carries `door`: a
  client there already has the door's address, and the door, which strangers
  can reach, doesn't hand out the machine's own addresses.
- **What the hub can't see.** The hub reports its own listener. Behind a
  port mapping the reachable address is different: Docker publishes the
  door's 7371 as host port 7378 by default (`compose.yaml`), and a router
  forward or a tunnel has its own address. Before putting an address in a
  setup code, check that it answers `/healthz`.
- **`advertise` (v2.0.2).** The operator can say where clients reach the
  door with `HUB_PUBLIC_ADVERTISE` (this host's address and the published
  port, or a tunnel's URL). `door` then carries it beside the listener's own
  address: `"door": {"port": 7371, "bind": "0.0.0.0", "advertise":
  "100.64.1.2:7378"}`. Prefer it when present; it is the operator's word,
  not something the hub checked, so check it answers `/healthz` too.

`/healthz` lists the feature as `door`.

## Presence: a client that hangs up goes offline soon (v2.0.2)

An address counts as online while it holds a parked poll or sync, and for
90 seconds after its last call (as in v1). Two things changed, so a client
that stops shows as gone within seconds instead of minutes:

- **A hang-up ends it.** When a parked poll or sync ends because the client
  closed the connection (its process stopped, was killed or crashed), the
  address stays online only for a **10-second grace** after that, long
  enough for a poller to restart or retry. Calling or parking again within
  it keeps the address online. A poll or sync that answers, and every other
  call, keeps the 90-second window, so a client between calls is not shown
  as gone.
- **A device in use hears at once.** When the set online changes (a grace
  or window ran out, an address came back), the hub wakes parked syncs
  within a second. The sync of a device in use (`POST /api/devices/active`,
  see "Active devices") answers at once, carrying `online`. A device not in
  use is not woken for it: it gets `online` with its next answer (mail, or
  the end of its wait), or at once when it reports itself in use, because
  that wakes its parked sync. Reporting itself no longer in use wakes it
  too, so it stops being woken for presence at once (v2.0.3).
- **Last seen as it stands (v2.0.3).** The roster's `last_seen` is the
  address's last call, but a sync sends a roster entry only when the entry
  changes. So when an address goes offline the hub marks its entry changed:
  the answer that reports it gone, and every device's next answer, carry
  the entry with `last_seen` its last call, not the time it registered.

So a client that stops shows as offline on a device in use within about
11 seconds (the grace, plus a second), and coming back shows within about a
second. A client that vanishes without closing its connection (network loss,
sleep) is not noticed until its parked call ends at its wait, and then the
window applies: about 2 minutes for Orgtree's 25-second polls, about 2.5 for
Hubchat's 55-second syncs. (TCP keepalive could notice sooner, but its
probes, every few seconds on every parked connection, would keep waking
phones' radios; the hub does not use it.) v1 polls are unchanged: their
roster carries the same online flags, as of each answer.

## Telling what a hub supports

`/healthz` also reports `"version"` (`"2.0.3"`), `"features"` and `"now"`
(the hub's clock, unix milliseconds). The features are the additions this
hub serves: `person`, `profile`, `reply_to`, `sync`, `devices`, `history`,
`delete`, `long_messages`, `message_limit`, `directory`, `uploads`, `link`,
`device_keys`, `active`, `lazy_history`, `door`. A v1 hub reports none of
them. On the main port, `/healthz` also says where the relay-only door
listens (`door`, when one runs).

## The hub's version, to every client

Every answer that names the hub (its `"name"`) also carries
`"version"`: the hub's own version as a string (`"2.0.3"`), the same value
`/healthz` and `orgtree-mailhub --version` report. So a client sees it on
the paths it already uses, without a separate call:

| Answer | Route |
|---|---|
| registration | `POST /api/register` |
| poll (the v1 long poll) | `POST /api/poll` (and `GET`) |
| sync (every device gets everything) | `POST /api/sync` |
| roster | `GET /api/roster` |
| directory | `GET /api/directory` |
| the operator page's data | `GET /ui/data` (the page shows it beside the hub's name) |
| health | `GET /healthz` |

The key sits beside `"name"` at the top level of each answer. A v1 hub sends
no `"version"` anywhere: a client shows its version as unknown.

## Optional UnifiedPush (feature `unifiedpush`)

Android clients can opt in to distributor-delivered wake-ups. The background
connection remains the default. The hub sends directly to the device's
registered distributor endpoint; no project-hosted gateway is involved.

`POST /api/push` takes `device_id`, `endpoint`, `p256dh`, `auth` and an optional
`slug` when several addresses are authenticated. The device must already have
synced or enrolled and must not be revoked. Authentication follows sync: a shared
identity credential controls its devices; a device-signed credential controls
only that device. `p256dh` is an uncompressed P-256 public key (65 bytes) and
`auth` is a 16-byte secret, both base64url without padding. Identical registration
is idempotent; if a wake is waiting to retry it resets the failure backoff while
respecting the five-second attempt cooldown. A new or replaced registration queues one initial wake, covering
mail already waiting. The answer is only `{"registered":true}`.

`DELETE /api/push?device_id=...&slug=...` removes the subscription and answers
`{"registered":false}`, including if it was already absent. Revoking a device
and unregistering/removing an address also remove the subscription. Capabilities
never appear in device or operator listings. As with any already-started HTTP
request, one in-flight wake can arrive after unregistration.

The payload is the fixed bytes `wake`, encrypted using RFC8291 (`aes128gcm`). It
contains no message content, address, sender, message ID or count. Hubchat fetches
mail using normal authenticated sync. The hub uses `TTL: 86400` and a constant
collapse topic. No VAPID key is advertised; distributors requiring VAPID cannot
be used by this implementation.

New received mail queues a wake in its commit transaction. Outbound requests run
separately, with four requests at most in flight, one per canonical destination
host, and one coalesced pending wake per subscription. A completed request frees
its slot immediately; another host's slow request does not hold up a batch.
There is a five-second cooldown after successful delivery and endpoint changes
cannot bypass the five-second minimum since the last attempt.
Devices with a running `active_until` lease receive no push: their live sync
already receives mail. A pending wake becomes eligible when they report inactive
or the lease expires (at most 90 seconds if the app crashes).
Retries back off from ten seconds to at most one per hour and survive restarts;
new mail does not bypass a pending retry's backoff. HTTP 404/410 deletes the
subscription. Other failures retain it. Endpoint replacement cannot be erased
by the old endpoint's late response. No response body is consumed.

By default, endpoints must be publicly routable HTTPS URLs, without credentials or fragments,
at most 1000 bytes. Loopback, LAN, tailnet, link-local and other special IP ranges
are refused, including through DNS. Every delivery resolves and pins validated
addresses; redirects and environment proxies are disabled. Connect timeout is
three seconds; the entire delivery attempt has a four-second deadline. DNS work
is bounded to eight OS lookups, including timed-out lookups still running in the
resolver. Registration releases its database connection before DNS and answers
503 when resolver capacity is busy. Other DNS failures have one generic response.
Registration alone does not mark the address online; normal sync does that.

A tailnet-only **hub** works if it can reach the distributor's public server.
The device must still be able to reach its hub when woken (for example, Tailscale
must stay enabled). A push cannot itself restore that network path. For a
self-hosted distributor on a LAN or tailnet, the operator may set
`HUB_PUSH_ALLOW` to a comma-separated list of exact hostnames or CIDRs, for example
`ntfy.example.ts.net,100.64.5.6/32`. These destinations may resolve to private
addresses. HTTPS with valid TLS, DNS pinning, no redirects and all timeouts still
apply. Keep entries narrow: an allowed hostname authorizes whichever addresses it
resolves to, and a CIDR authorizes any endpoint in that range. Restart the hub after
changing its allowlist. There are no wildcard or suffix host matches.

TLS uses bundled Mozilla roots, not the OS trust store; a private-CA certificate
is not accepted. NAT64 translation prefixes are refused unless explicitly
allowlisted. Globally routed IPv6 addresses can belong to a home LAN too; HTTPS
certificate verification still applies, but the hub can attempt a TCP connection
to those public addresses. Allowlists may authorize loopback or link-local
addresses, so use only narrowly scoped entries you trust.
