-- mail hub v2.5.1, schema 14: Web Push for browsers (Hubchat Web).
--
-- A browser's subscription carries what a UnifiedPush one has not: the
-- signing key the browser made for it (`vapid`: a capability like `auth`,
-- never logged, listed or returned), the web app's own address a
-- notification opens (`navigate`), whether a notification may carry the
-- message's text (`preview`), and the newest message already announced
-- (`notified_until`, a `messages.n`), so each message is announced once.
-- Rows from before are UnifiedPush and behave as before.
ALTER TABLE device_push
  ADD COLUMN kind text NOT NULL DEFAULT 'unifiedpush' CHECK (kind IN ('unifiedpush', 'webpush')),
  ADD COLUMN vapid bytea,
  ADD COLUMN navigate text,
  ADD COLUMN preview boolean NOT NULL DEFAULT true,
  ADD COLUMN notified_until bigint,
  ADD CHECK ((kind = 'webpush') = (vapid IS NOT NULL AND navigate IS NOT NULL AND notified_until IS NOT NULL)),
  ADD CHECK (vapid IS NULL OR octet_length(vapid) = 32),
  ADD CHECK (navigate IS NULL OR octet_length(navigate) <= 300);
