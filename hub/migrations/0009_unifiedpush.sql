-- One secret subscription and one coalesced wake per enrolled device.
-- No message content or message identifiers enter this table.
CREATE TABLE device_push (
    slug text COLLATE "C" NOT NULL,
    device_id text COLLATE "C" NOT NULL,
    registration text NOT NULL,
    endpoint text NOT NULL,
    p256dh bytea NOT NULL,
    auth bytea NOT NULL,
    pending bigint NOT NULL DEFAULT 1,
    next_attempt timestamptz,
    attempts integer NOT NULL DEFAULT 0,
    last_sent timestamptz,
    PRIMARY KEY (slug, device_id),
    FOREIGN KEY (slug, device_id) REFERENCES devices (slug, device_id) ON DELETE CASCADE,
    CHECK (octet_length(endpoint) <= 1000),
    CHECK (octet_length(p256dh) = 65),
    CHECK (octet_length(auth) = 16)
);
CREATE INDEX device_push_due ON device_push (next_attempt) WHERE next_attempt IS NOT NULL;
