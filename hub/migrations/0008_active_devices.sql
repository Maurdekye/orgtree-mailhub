-- mail hub v2.0.0, schema 8: active devices. The device a person is using
-- says so (POST /api/devices/active), and the address's other devices see
-- it in their sync answers, so they can leave notifications to it. It
-- lapses by itself ACTIVE_FOR after the last "active": a device that
-- crashes or sleeps stops counting without any sweep.
ALTER TABLE devices ADD COLUMN active_until timestamptz;
