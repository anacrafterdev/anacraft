# `craft mcp` — the MCP server

Stdio by default: the client spawns `craft mcp` as a child process and talks
newline-delimited JSON-RPC over its pipes. Stdout belongs to the protocol, so
everything human-facing goes to stderr — never pipe stdout anywhere while the
server is running.

A client that **cannot spawn a child process, or cannot reach the binary and
the credentials**, gets the same server over HTTP instead — see
[Over HTTP](#over-http) below.

Speaks MCP revisions `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`,
echoing back whichever the client asks for. The tool surface is the same in all
four. A client on `2025-11-25` also gets the anacraft mark in the handshake, as
an inline `data:` PNG on `serverInfo.icons`, so a connector list can draw it
without fetching anything; older revisions have no field for it and are not
sent one. Whether the icon is drawn is the client's call — some show a letter
placeholder regardless.

## Wiring it up

**Claude Desktop and Smartloop** — `craft mcp --install` writes the Claude
block, merges with whatever else is in that file, and writes the Smartloop
TOML definition; add `--demo` to write servers that serve
synthetic data. Either way it is the one `anacraft` entry, so re-running
`--install` without `--demo` upgrades it in place rather than leaving a
synthetic twin alongside the real one. `craft mcp --uninstall` removes the
Claude entry. Restart the apps afterwards. To do it by hand:

| Platform | File |
|---|---|
| macOS | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| Linux | `~/.config/Claude/claude_desktop_config.json` |
| Windows | `%APPDATA%\Claude\claude_desktop_config.json` |

```json
{
  "mcpServers": {
    "anacraft": { "command": "/usr/local/bin/craft", "args": ["mcp"] }
  }
}
```

Absolute path, deliberately: a desktop app is not launched from a shell and
does not inherit the `PATH` where `craft` works fine. `which craft` gives the
value to paste.

**Claude Code** — `claude mcp add anacraft -- craft mcp`.

**Anything else** — same command, same args. Add `--demo` to any of them to
serve synthetic data with no account and no subscription, which is the fastest
way to prove the client-side wiring before blaming credentials. The demo says so
at the handshake and stamps `synthetic: true` on every answer, so an assistant
reading it knows not to quote the numbers as real.

## Over HTTP

`POST /v1/mcp` on `craft serve` is the same server, reached over a socket
instead of a pipe. It exists for the clients that cannot spawn `craft mcp`:

- a **strictly confined snap** — the `home` AppArmor interface excludes
  top-level hidden directories, so `~/.local/bin/craft`, `~/.anacraft/` and
  `~/.config/anacraft/` are all unreadable from inside it;
- a **Mac App Store** build, walled into `~/Library/Containers/<id>/Data` with
  no entitlement that would grant the rest of the home directory;
- a **container** with no home directory worth the name.

All of them can open a loopback socket, and none of them needs the credentials
to do it. The process stays outside the sandbox, reading `~/.anacraft/` as
usual; the client is handed a URL and a bearer token.

```
craft serve --port 7777 --token "$(openssl rand -hex 20)" --idle 0 --no-open
```

Then point the client at `http://127.0.0.1:7777/v1/mcp`:

```
claude mcp add --transport http anacraft http://127.0.0.1:7777/v1/mcp \
  --header "Authorization: Bearer <token>"
```

Pass `--port` and `--token` explicitly. Without them the OS picks a port and
the token is minted fresh each run, so the URL a client was configured with
stops working the next time the server starts. `--idle 0` keeps it up; the
default stops after 60 idle minutes.

What the transport does and does not do:

- **Answers are immediate JSON.** No event stream and no session id: nothing
  here ever speaks first, so `GET /v1/mcp` refuses with `405` rather than
  holding a socket open for traffic that is never coming.
- **A notification gets `202` and an empty body**, as the transport spells it.
- **A JSON-RPC batch is answered**, though `2025-06-18` withdrew them, so a
  client on an older revision is not met with silence.
- **The bearer token and the `Origin` check are the server's own** — the same
  guard every other route is behind, which is what the transport asks of an
  HTTP server on loopback. A page on the public web cannot reach it.
- **Locking works the same way.** A missing plan or login is a tool error
  carrying the reason, not an HTTP refusal — and because the server is rebuilt
  while it is locked, signing in through the `craft serve` page unlocks it
  without a restart.
- **Bad JSON comes back as `-32700` with HTTP 200**, because the message was
  malformed, not the transport.

## Before it will serve

Two preconditions, both checked at startup so the reason is known before the
first tool call:

1. **The Elite plan.** `craft mcp` is what Anacrafter **Elite** ($9.99/month) is
   for — `craft subscribe` for the starter, `--plan elite` for this. The config
   carries it as `supporter = true` and `tier = "elite"`. `--demo` skips this.
2. **A stored token.** `craft login`, run in a terminal by a person. The server
   will not start an OAuth flow — a browser consent screen inside a client's
   subprocess is not something an agent can complete. `configure_site`, the one
   write, works off that same stored grant (a fresh `craft login` carries the
   `analytics.edit` scope it needs) instead of opening a browser of its own.

Neither is fatal. Missing one **locks** the server rather than exiting it: the
reason goes to stderr — the client's log — the handshake still succeeds, the
tools are still listed, and every call comes back as a tool error carrying that
reason. This is deliberate. A process that exits during startup reaches the user
as `Server disconnected`, which points at the pipes instead of the subscription;
a locked server tells the assistant what to say. The handshake `instructions`
carry the reason too, so it can be relayed before anything is called.

## The tools

Every report tool takes an optional `property` (numeric GA4 id) and falls back
to the saved default, so an assistant that knows nothing about the config still
gets answers. `days` is clamped to 1–365 and defaults to whatever the tool's own
schema advertises — 7 for the reports, 28 for `audit_site`, because a key event
that has not fired yet this week is not the same as one that is broken. `limit`
defaults to 10 and is clamped to 1–100. `configure_site`, the one write, takes a `domain`
instead — creating the property is the point, so there is none to point at yet;
it also takes an optional `account`, `timezone` and `currency` (default `USD`).

| Tool | Arguments | Answers |
|---|---|---|
| `site_status` | `days` | Headline metrics against the period before, the daily user series, and the achievements that fired |
| `audit_site` | `days` (28) | Twelve graded checks on how the property is measuring — key events, revenue tagging, double counting, self-referral, events that stopped firing |
| `live_visitors` | — | Active users in the last 30 minutes, by country |
| `list_pages` | `days`, `limit` | Most-visited pages, by views |
| `list_events` | `days`, `limit` | Events by count, plus the per-day total against the previous period |
| `list_referrers` | `days`, `limit` | The URLs sending traffic, by sessions |
| `list_traffic_sources` | `days`, `limit` | GA4 source / medium pairs, by sessions |
| `list_countries` | `days`, `limit` | Countries, by users |
| `list_properties` | — | Every property this account can read, and which is default |
| `search_pages` | `query`\*, `days`, `limit` | Pages whose path contains a substring |
| `search_events` | `query`\*, `days`, `limit` | Events whose name contains a substring |
| `configure_site` | `domain`\*, `account`, `timezone`, `currency` | Creates a property and web stream for a domain and returns the gtag.js snippet |

\* required. `query` matches case-insensitively; `domain` belongs to
`configure_site`, the one writer — every other tool is read-only.

Which tool answers which question:

- "How is the site doing?" · "Are we up or down this week?" → `site_status`
- "That number looks wrong." · "Is our tracking set up properly?" · "Why are
  conversions zero?" → `audit_site`. Also worth running unprompted before
  leaning hard on any other tool against a property you have not seen before:
  a site with nothing marked as a key event reports zero conversions
  truthfully, and saying so is a different answer from "conversions fell".
- "Who's on it right now?" → `live_visitors`
- "Which pages are doing well?" · "How did the blog do?" → `list_pages`,
  then `search_pages` with `/blog`
- "Where is the traffic coming from?" → `list_traffic_sources` for the channel
  mix, `list_referrers` for the actual links
- "Did the signup flow get used?" → `search_events` with `signup`
- "We're not tracking a site yet — set it up." → `configure_site` with the
  `domain`. It says whether it created the property (`created`), finished one
  that was already started (`finished`), or found one (`reused`) — and never
  changes the saved default, so follow up with `craft use` only if that is what
  is wanted.

## What comes back

Structured JSON, not the rendered panels — labelled numbers with their units.
Every answer names the property and the window it covers, so quote the window
alongside any number taken from it.

```json
{
  "property": "397412345",
  "property_name": "anacraft.dev",
  "date_range": {
    "start_date": "7daysAgo",
    "end_date": "yesterday",
    "days": 7,
    "note": "GA4 relative dates; the window ends yesterday, because today is still partial"
  }
}
```

On top of that envelope:

- **`site_status`** — `has_data`, `metrics[]` (`metric`, `label`, `unit`,
  `value`, `previous`, `change_pct`), `daily_users[]` (`date`, `users`),
  `achievements[]` (`title`, `detail`).
- **`audit_site`** — `clean`, `checks_run`, `checks_available`, `counts`
  (`critical` / `warning` / `note`), and `findings[]` of `check` (a stable
  slug), `grade`, `headline`, `detail` and `evidence`, worst first. A check that
  could not run — the Admin API unreadable, or the property too quiet for a
  share to mean anything — lowers `checks_run` rather than passing, so `clean`
  with `checks_run` below `checks_available` means "nothing found in the ten it
  could do", not "nothing wrong". Report the `detail`: repeating a slug at
  somebody explains nothing.
- **the ranked tools** — `dimension`, `metric`, `returned_total`, and `rows[]`
  of `name` / `value` / `share_of_returned`. That share is of the rows returned,
  not of the site: a top-ten list is a slice, and a percentage that quietly
  means something else is worse than none.
- **`list_events`** — the ranked shape plus `total_events`,
  `total_events_previous_period`, `change_pct`, and `daily[]`.
- **`live_visitors`** — `active_users` and `by_country[]`, with
  `"window": "the last 30 minutes"` in place of a date range.
- **`list_properties`** — `properties[]` only, with no property or window: it is
  not a question about one site.
- **`configure_site`** — no property or window on it (there was none to ask
  about). Instead `host`, `property`, `property_name`, `status`
  (`created` / `finished` / `reused`), `measurement_id`, `default_uri`, the
  `tag` to paste, a `note` saying it was not saved as the default, and — when
  a new property was made — `timezone` and `timezone_note`.

Two flags worth reading:

- `"cached": true` — the same question was asked within the last minute
  (ten seconds for `live_visitors`) and this is the stored answer. Quota is
  shared with the dashboard, and an agent in a loop can out-ask a human by
  orders of magnitude. A number that will not move usually means this, not a
  site that went quiet.
- `"synthetic": true` — `--demo`. Never present these numbers as the real site.

A failed report comes back as tool output with `isError` set, not as a
transport fault, so the reason — a rejected property id, an expired login — is
readable and actionable rather than looking like a broken server.

## What it will not do

Every report is a read. Nothing in the server opens an OAuth flow or changes
the default property: `craft login` and `craft use` stay human-only commands.
An agent cannot silently repoint the tool at another property — it can only
pass `property` on a single call. `configure_site` is the one exception, and
even it keeps its hands off the config: it creates the property and web stream
on Google's side and returns the tag, but never saves it as the default.
