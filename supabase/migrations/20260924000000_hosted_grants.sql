-- What app.anacraft.dev remembers about a user once their browser session is
-- gone: the Google grant their MCP connector reads with.
--
-- One row per Google account. The refresh token is never stored as itself:
-- `refresh_sealed` is ChaCha20-Poly1305 ciphertext under a key that lives in
-- GCP Secret Manager and nowhere in this database, so the table on its own
-- opens nothing. The connector link is not stored either — only its SHA-256,
-- which is how a link presented to /v1/mcp finds its row.
--
-- Only the hosted server reaches this table, with the service key. The anon
-- key every CLI ships with is locked out entirely.

create table if not exists public.hosted_grants (
  -- Google's stable account id, the same key as `users.user_id`.
  user_id        text primary key,
  email          text,
  refresh_sealed text        not null,
  connector_hash text        not null unique,
  created_at     timestamptz not null default now(),
  updated_at     timestamptz not null default now()
);

alter table public.hosted_grants enable row level security;
revoke all on public.hosted_grants from anon, authenticated;
