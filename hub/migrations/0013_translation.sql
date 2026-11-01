-- mail hub v2.4.0, schema 13: translation between people (docs/v2-additions.md,
-- "Translation"). A person may read in a language (`identities.language`, a
-- BCP 47 tag; people only, told to nobody else). Mail to them in another
-- language is translated after it is delivered: a job per message waits in
-- `translate_jobs`, and the outcome stays beside the message in
-- `message_translations`, which only the reader's sync and history carry.
-- `translation_spend` counts what a month cost, for the hub's limit.
-- A hub without schema 13 refuses a database that has it ("newer than this
-- hub understands"): going back means restoring a backup from before
-- (docs/OPERATIONS.md).

ALTER TABLE identities ADD COLUMN language text;

CREATE TABLE translate_jobs (
  message_n    bigint PRIMARY KEY REFERENCES messages (n) ON DELETE CASCADE,
  lang         text NOT NULL,
  source_lang  text NOT NULL,
  attempts     integer NOT NULL DEFAULT 0,
  next_attempt timestamptz NOT NULL,
  created_at   timestamptz NOT NULL
);
CREATE INDEX translate_jobs_due ON translate_jobs (next_attempt);

CREATE TABLE message_translations (
  message_n     bigint PRIMARY KEY REFERENCES messages (n) ON DELETE CASCADE,
  lang          text NOT NULL,
  source_lang   text,
  status        text NOT NULL CHECK (status IN ('translated', 'same', 'too_long', 'limit', 'failed')),
  body          text,
  model         text,
  input_tokens  bigint,
  output_tokens bigint,
  cost_micros   bigint,
  created_at    timestamptz NOT NULL
);

CREATE TABLE translation_spend (
  month       text PRIMARY KEY,  -- UTC, YYYY-MM
  cost_micros bigint NOT NULL DEFAULT 0,
  messages    bigint NOT NULL DEFAULT 0
);
