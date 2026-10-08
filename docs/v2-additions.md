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

## Telling what a hub supports

`/healthz` also reports `"version"` (`"2.0.0"`) and `"features"`, the
additions this hub serves: `person`, `profile`, `reply_to`, `sync`,
`devices`, `history`, `delete` (more as later parts land). A v1 hub reports
neither.
