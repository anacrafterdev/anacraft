-- The hosted MCP connector is gone, and with it the one thing that kept a
-- user's Google grant after their browser session ended. The pages moved to
-- anacraft.dev/app, which signs in in the browser and keeps nothing, and MCP
-- is `craft mcp` on the user's own machine.
--
-- Dropping the table deletes every sealed refresh token in it. The key that
-- opened them was in GCP Secret Manager and goes with the hosted server.

drop table if exists public.hosted_grants;
