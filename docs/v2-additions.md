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
  to update (slug)`. A `slug` the header does not sign in: 401
  `no valid credentials for that address`. No valid credentials: 401
  `no valid org credentials`.
- 422 refusals: `name must be a string`, `about must be a string`,
  `name is longer than 48 characters`, `about is longer than 200 characters`,
  `nothing to update: give name and/or about`.
- The address, username and kind never change here. The change counts as
  activity (`last_seen`), and every client sees it in the roster of its next
  poll (and, from slice 2, through sync).
- Registering again still sets the name and about line from the register
  body, as in v1: a client that edits its profile should send the current
  values when it registers.

## Replies (G3)

`/api/send` accepts an optional `reply_to`: the id of the message this one
answers. The hub stores it as given and returns it unchanged in poll (and the
operator view `/ui/messages`; from later slices also sync and history). It is
never checked: the message it names may live on another hub or be gone.

- A message sent without `reply_to` (or with `null`) has exactly v1's
  envelope; with one, `"reply_to"` is the envelope's last key.
- A duplicate send (same id) keeps the first payload's `reply_to`, as it keeps
  the rest of the first payload.
- 422 refusals: `reply_to must be a message id (a string)`,
  `reply_to is longer than 4096 characters`, `reply_to contains a NUL
  character`. (v1 ignored the key, so a v1-era client that sent a non-string
  `reply_to` would now be refused; none does.)
