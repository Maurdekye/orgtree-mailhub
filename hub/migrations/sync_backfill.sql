-- Gives every message of a registered address that has no change-log entry
-- one, after that address's existing entries and in message order, then
-- moves the heads up. Idempotent: run after schema 3 is created and after
-- every import, never while the hub serves (the hub runs it before it opens
-- its listeners).
INSERT INTO mailbox_log (slug, seq, message_n)
SELECT x.slug, COALESCE(h.seq, 0) + row_number() OVER (PARTITION BY x.slug ORDER BY x.n), x.n
  FROM (SELECT to_slug AS slug, n FROM messages UNION SELECT from_slug, n FROM messages) x
  JOIN identities i ON i.slug = x.slug
  LEFT JOIN mailbox_heads h ON h.slug = x.slug
 WHERE NOT EXISTS (SELECT 1 FROM mailbox_log l WHERE l.slug = x.slug AND l.message_n = x.n);

INSERT INTO mailbox_heads (slug, seq)
SELECT slug, max(seq) FROM mailbox_log GROUP BY slug
    ON CONFLICT (slug) DO UPDATE SET seq = GREATEST(mailbox_heads.seq, EXCLUDED.seq);
