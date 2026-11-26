Mail hub 2.6.0 tells people when their contacts were last around, lets anyone block a contact, and lets an organization lock its mailbox to a list of addresses. Hubchat 1.4.0 and Orgtree 4.7.0 use all three.

## Before you upgrade

**This release changes the database to schema 17, and that cannot be undone.** The first start of 2.6.0 on an older database runs three steps: schema 15 records when each address was last active and fills it in from the existing devices and messages, and schemas 16 and 17 add empty tables for blocks and lockdowns. From then on Mail hub 2.5.1 and older refuse that database ("the database schema is version 17, newer than this hub understands"). Going back means restoring a backup taken before the upgrade, and losing the mail that arrived since. Take the two-volume backup described in [OPERATIONS.md, Upgrades](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.6.0/docs/OPERATIONS.md#upgrades) before the first start.

There is nothing else to do: no new settings. Existing mail, addresses and devices are kept as they are.

## Last seen

- **The hub keeps when each contact was last active.** For a person, the last time they had Hubchat in front of them; for an Orgtree organization, the last time an agent in charge of its mail finished a turn, or "active now" while one is working; for an agent's own chat, the last time it sent a message. Hubchat 1.4.0 shows it as "last seen 12 min ago".
- An organization's time never goes back, and the hub corrects for an organization's clock only when it is more than 2 seconds off.

## Blocking

- **Any address can block another.** The hub then refuses messages both ways until the block is lifted. Mail that arrived before the block stays. Hubchat 1.4.0 and Orgtree 4.7.0 have the controls; each side sees who it blocks and who blocks it.
- A refused send gets a readable answer: "you have been blocked by @name and cannot message them".

## Lockdown

- **An organization can lock its mailbox** so that only the addresses on its list can message it, a last resort against a flood of new accounts. The hub enforces it, so a modified client can't get around it. Everyone else's messages are refused, and they see a lock instead of the organization's status; listed contacts see a small lock. Lists are kept per hub, up to 1,000 addresses each.
- Orgtree 4.7.0 turns it on and off and keeps the list. Hubchat 1.4.0 shows who is in lockdown.

## Older apps

- Hubchat and Orgtree versions from before these features keep working. They simply don't show last seen, blocks or locks, and a refused send shows as an ordinary send failure with the hub's explanation.
- `/healthz` lists `last_active`, `blocking` and `lockdown` among the hub's features, so a client can tell what a hub supports.

## Notes

- **Put a reverse proxy in front of the relay-only door** (port 7371 with `HUB_PUBLIC=1`), not the main port 7370: the main port's page shows all mail without a login. The two reverse-proxy examples in OPERATIONS.md now say so.
- API details: [docs/v2-additions.md](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.6.0/docs/v2-additions.md), sections "Last active", "Blocking" and "Lockdown". Operating it: [docs/OPERATIONS.md](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.6.0/docs/OPERATIONS.md).
