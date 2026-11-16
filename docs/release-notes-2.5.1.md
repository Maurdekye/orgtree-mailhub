Mail hub 2.5.1 sends notifications to browsers, for Hubchat Web.

## Before you upgrade

**This release changes the database to schema 14, and that cannot be undone.** The first start of 2.5.1 on an older database adds five columns to `device_push`. From then on Mail hub 2.5.0 and older refuse that database ("the database schema is version 14, newer than this hub understands"). Going back means restoring a backup taken before the upgrade, and losing the mail that arrived since. Take the two-volume backup described in [OPERATIONS.md, Upgrades](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.5.1/docs/OPERATIONS.md#upgrades) before the first start.

Existing mail, addresses, devices and UnifiedPush subscriptions are kept as they are.

## Web Push for browsers

- **Notifications for Hubchat Web.** A browser that turns notifications on in Hubchat Web (Hubchat 1.3.7 or later) registers with the hub, and the hub then sends that browser a notification for new mail, also while Hubchat is closed. Nothing changes for the Hubchat apps or for Orgtree.
- **Encrypted on the way.** The notification travels encrypted from the hub to the browser. The browser's push service (Google's, Microsoft's, Mozilla's or Apple's) sees only where it goes, when, and how large it is.
- **When nothing is sent.** A notification is held while any of the person's devices is in use, is not sent for mail already read elsewhere, and is not sent to someone whose status is Do not disturb. A person can also choose notifications without the sender and the text.
- **New setting `HUB_PUSH_CONTACT`:** a `mailto:` or `https://` address a push service is given with each push, should it want to reach whoever runs the hub. Empty: the project's repository address. A value that is neither stops the hub at the start with a line that names it.
- **Outbound access.** The hub must be able to reach the browsers' push services over HTTPS (for example `fcm.googleapis.com`, `updates.push.services.mozilla.com`, `*.push.apple.com`). `HUB_PUSH_ALLOW` needs no entry for them.
- `/healthz` lists `webpush` among the hub's features, so a client can tell whether a hub can send them.

Seen working by hand in Chrome on Android and on Windows. Not tried: Safari, an iPhone and Firefox.

## Container image

- The published image `ghcr.io/maurdekye/orgtree-mailhub` (amd64 and arm64) is now pushed as OCI images under an OCI index that carries the package's description, so the package page on GitHub shows what it is. Pulling and running it is unchanged: `2.5.1` and `latest` are the tags.
- The release's files still carry the image for `docker load` and the checksums.

## Notes

- Hubchat Web also needs its own address in `HUB_WEB_ORIGINS` (since 2.5.0).
- API details: [docs/v2-additions.md](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.5.1/docs/v2-additions.md), section "Web Push for browsers". Operating it: [docs/OPERATIONS.md](https://github.com/Maurdekye/orgtree-mailhub/blob/v2.5.1/docs/OPERATIONS.md).
