-- 2026-10-12 SPEC §3.4 amendment: interrupted turns. Optional flag,
-- default 0 (old rows are all un-interrupted).
ALTER TABLE conversation_turns ADD COLUMN interrupted INTEGER NOT NULL DEFAULT 0;
