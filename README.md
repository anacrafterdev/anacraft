<h1 align="center">⛏ craft</h1>

<p align="center"><b>Google Analytics, mined block by block.</b></p>

<p align="center">
  Sets Google Analytics 4 up for a domain in one command, then reads it back as
  a terminal dashboard — seven live panels, ore-textured bars, a realtime event
  feed, and achievement toasts when the numbers move.
</p>

<p align="center">
  <a href="https://anacraft.dev">anacraft.dev</a> ·
  <a href="https://github.com/mehfuzh/anacraft/releases">Releases</a> ·
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache%202.0-blue.svg"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust 1.74+" src="https://img.shields.io/badge/Rust-1.74+-orange.svg"></a>
</p>

<p align="center">
  <img src="docs/dash.png" alt="The anacraft dashboard: seven panels showing GA4 metrics in a terminal" width="960">
</p>

<p align="center"><sub><code>craft dash --demo</code> · osaka-jade · 132×52</sub></p>

## Install

**macOS / Linux**

```sh
curl -fsSL https://anacraft.dev/install.sh | bash
```

Installs to `/usr/local/bin` when that is writable, otherwise `~/.local/bin` —
never with `sudo`. Set `INSTALL_DIR` to choose somewhere else:

```sh
curl -fsSL https://anacraft.dev/install.sh | INSTALL_DIR=~/bin bash
```

**From source**

```sh
cargo install --git https://github.com/mehfuzh/anacraft
```

**Manual** — grab a binary from [Releases](https://github.com/mehfuzh/anacraft/releases), extract it, and put `anacraft` on your `PATH`.

## Quick start

`anacraft` with no command opens the dashboard — `dash` is the default. With no
property saved it runs on synthetic data, so it works before you sign in.

```sh
# The dashboard, on synthetic data — no Google account needed
craft

# No GA4 property yet? One command creates it and prints the tag (★ Anacrafter)
craft configure yoursite.com

# Already have one? Connect it instead
craft login        # OAuth sign-in
craft props        # list the properties this account can read
craft use 1234567  # save it as the default

# Same bare command, now against your property
craft
```

`craft configure` is part of the [Anacrafter plan](https://anacraft.dev/pricing.html)
($2.99/month; the plans above it each add one more thing).
It creates the property and its web data stream, prints the gtag.js snippet with
your measurement id already in it, and saves the property as the default. The
subscription ask arrives on the page the Google sign-in already ends on, and the
terminal picks the payment up from there — nothing is created in your Analytics
account before it clears. Paste the snippet into `<head>`, then `craft live` to watch the
first visit arrive.

Run it again for the same domain and it creates nothing — it finds the property
already measuring that site and prints its tag again. This is the only command
that changes anything in your Analytics account, so it asks Google for
permission to do so when you run it, and never at sign-in; see
[docs/oauth-scopes.md](docs/oauth-scopes.md).

### One-shot reports

Not everything needs a dashboard. These print and exit.

```sh
craft overview --days 30   # headline metrics, deltas, achievements
craft pages                # most-visited pages
craft portals              # where traffic arrives from
craft realms               # traffic by country
craft live                 # who is on the site right now
craft demo                 # render an overview from synthetic data
```

Two flags are global: `--property <id>` queries a property other than the saved
default, and `--theme <name>` renders with a palette other than the saved one.

### Piping the numbers somewhere

`overview` takes `--format`, so the same report a person reads as panels can
also leave the terminal as data.

```sh
craft overview --format json            # one object, one line — for jq or a script
craft overview --format slack           # a Block Kit payload, for a webhook
```

`json` answers in the same shape as the `site_status` MCP tool: labelled
metrics with their unit, the previous period, the percentage change, the daily
user series, and the achievements that fired. The window is reported as the
first and last day the API actually returned rather than computed here — GA
resolves `last 7 days` in the property's timezone, which is not necessarily
this machine's.

`slack` wraps the same numbers as blocks. Both print the payload and nothing
else, so a weekly digest is one cron line:

```sh
0 9 * * 1  craft overview --days 7 --format slack \
             | curl -sX POST -H 'Content-Type: application/json' -d @- "$SLACK_WEBHOOK"
```

Neither format needs a subscription — they render a report `craft overview`
already prints for free.

### Claude Desktop

```sh
craft mcp --install           # write Claude Desktop and Smartloop configs
craft mcp --install --demo    # ...pointed at synthetic data instead
craft mcp --uninstall         # take it back out again
```

Restart Claude Desktop and ask it how the site is doing. The block it merges in
leaves any other servers alone:

```json
{
  "mcpServers": {
    "anacraft": { "command": "/usr/local/bin/craft", "args": ["mcp"] }
  }
}
```

Needs `craft login` first and an active subscription — without either the
server still starts and its tools say which one is missing, so the client never
reports it as disconnected. `craft mcp --demo` runs on synthetic data without
either. More in [Ask an assistant](#ask-an-assistant).

## Audit

Every other command answers "what happened". `craft audit` answers the question
before it — is this property measuring the site at all, and is what it measured
worth trusting.

```sh
craft audit                  # twelve checks over the last 28 days
craft audit --days 90        # a longer window
craft audit --format json    # the findings as one object, for a script
craft audit --format slack   # a Block Kit payload, for a webhook
craft audit --demo           # a synthetic report — no account, no subscription
```

It reads two APIs, because measurement and configuration fail separately. The
Data API says how often `purchase` fired; the Admin API says whether anybody
ever told GA4 that `purchase` was the point. A property can pass the first and
fail the second for a year without anybody noticing, and that combination —
traffic arriving, nothing marked as an outcome — is the most common thing this
finds.

**What it checks.** Four things that make a number wrong:

- nothing recorded at all, which is a tag that is not installed or a property
  that is not the one the site reports to
- no web data stream, so there is no measurement id to put on a site
- nothing marked as a key event, or a key event configured and never fired —
  GA4 matches names exactly, so `Purchase` and `purchase` are two events and
  only one of them counts
- `purchase` arriving without its `value`, which makes every revenue, ARPU and
  ROAS figure on the property zero — including in any Google Ads account
  importing conversions from it

Four that distort one:

- page views counted twice, which is what a gtag snippet left in the page
  beside a GTM tag that also sends one looks like from here: bounce rate near
  nothing, views per session doubled
- the site referring itself, which is a visit cut in half by a domain the
  cross-domain configuration does not cover
- a payment or sign-in page credited with conversions, because the return trip
  starts a new session referred by the gateway
- an event that stopped firing between this window and the one before it, which
  is a tag removed, renamed, or moved behind something that no longer runs

And four that are worth knowing before reading anything else: two names for one
event, sessions GA4 could not attribute at all, a direct share high enough to
suggest campaigns going out untagged, and more than one site reporting into the
property.

**What it will not do.** It reports a symptom and names the usual cause, never
the other way round — "bounce rate is 1.2%" is something the API said, and "you
have two page_view tags" is a guess. It cannot see inside a GTM container, so
it finds the tagging bugs that show up in the data and not the ones that only
show up in the container. And every check has a floor under it: a property with
eighty sessions has no meaningful bounce rate and no meaningful direct share,
so those checks report as not run rather than firing on noise.

**Exit codes.** `0` when the property is clean, `2` when it is not, `1` on an
error — the same convention `craft watch` uses, so a weekly audit into Slack is
one cron line:

```sh
0 9 * * 1  craft audit --format slack \
             | curl -sX POST -H 'Content-Type: application/json' -d @- "$SLACK_WEBHOOK"
```

Unlike `craft watch`, a clean pass still prints: an audit is something somebody
asked for, and "twelve checks, nothing found" is the answer they asked for. The
line under every report says how many checks ran, because "no findings" means
nothing without the number of ways it looked — and a check that could not run,
because the Admin API was unreadable or the property was too quiet to judge, is
reported as not run rather than as a pass.

`craft audit` is part of the [Anacrafter Pro plan](https://anacraft.dev/pricing.html);
`craft audit --demo` is not, and shows the whole shape of a report before
anything is connected.

## Alerts

`craft watch` compares the most recent complete day against the mean of the
days before it and reports what moved further than it usually does. There is
nothing to configure for it to be useful: a site's own history is the
threshold.

```sh
craft watch                      # check once, print what moved, exit
craft watch --every 3600         # keep checking, hourly
craft watch --webhook "$HOOK"    # POST the alert to a Slack incoming webhook
craft watch --format json        # the same finding as one object, for a script
craft watch --demo               # synthetic alerts — no account, no subscription
```

Three things fire. A **drop** or a **spike** past the metric's threshold, and
**silence** — a count that went to nothing against a baseline that was not
nothing, which is what a removed tag or a site that is down looks like from
here. A window with no rows anywhere is reported once, as itself, rather than
as six metrics all going silent.

The defaults are per-metric, because conversions swing by a third on an
ordinary Tuesday and bounce rate does not: 30% for users, sessions and views,
40% for conversions, 25% for average session, 20% for bounce rate. A baseline
under 10 does not fire a count at all — on a site averaging four conversions a
day, one quiet day is a 25% "drop" that means nothing.

Any of it can be tuned per property:

```toml
[[property]]
id = "552157097"

  [property.watch]
  baseline_days = 28   # days the baseline averages over
  min_baseline  = 10   # a baseline under this never fires a count
  users         = 25   # % deviation that wakes somebody
  conversions   = 40
  bounce_rate   = 15
```

Keys are `users`, `sessions`, `views`, `conversions`, `bounce_rate`,
`avg_session`, or the GA4 API name if you prefer it. `--baseline <days>`
overrides the window for one run.

**What lands in Slack.** Each alert carries the metric, the day's value, how
far it moved, and the baseline it moved away from — plus a sparkline of the
whole window ending on the day being reported, because "38% below normal" does
not say whether the number slid all week or fell off a cliff last night. Where
one channel carries most of a move, it is named: *mostly Organic Search — 96
against 331 (79% of the move)*. Counts only, and only when that channel
accounts for at least 35% of the total movement — under that the move was
site-wide, and naming its largest slice would read as a cause. The message
carries a link back to the property in GA4, and a red or amber bar down its
side so an alert is told from everything else in the channel before a word of
it is read.

The same day's alert is only sent once. State lives in `~/.anacraft/watch.json`
and is keyed by the day being reported on, so `--every 3600` sends one message
about a drop rather than twenty-four, and a new day is news again. It is
recorded only after delivery succeeds — a webhook that was unreachable has
told nobody anything, so the next pass tries again.

**Exit codes.** `0` when nothing fired, `2` when something did, `1` on an
error. So a shell can decide for itself:

```sh
craft watch --format slack \
  || craft watch --format slack | curl -sX POST -d @- "$SLACK_WEBHOOK"
```

`--format slack` prints nothing at all on a quiet day, which is what keeps a
cron line from posting an empty message every hour. In a loop, `--webhook`
does the POST itself — a daemon has nothing to pipe into.

`--format` chooses what the webhook receives, so a URL pointed at something
other than Slack gets a shape it can read: `--format json --webhook <url>`
posts the JSON object. Two exceptions. Panels have no wire form, so leaving
`--format` alone and passing a webhook posts the Slack blocks. And a
`hooks.slack.com` URL always gets blocks whatever `--format` says, because
Slack answers a bare JSON object with `400 no_text` — the destination wins
over the flag there, which is what keeps `craft slack --install` from turning
`--format json` into an error.

### Installing into Slack

Making a webhook by hand is six steps in a developer console. `craft slack`
does it the way `craft login` does Google:

```sh
craft slack --install     # opens Slack; pick the workspace and channel there
craft slack --test        # post one message, to check it before an alert needs to
craft slack               # say where alerts currently go
craft slack --uninstall   # forget the webhook (the app stays installed in Slack)
```

Slack's own install screen carries the workspace and channel pickers, and the
`incoming-webhook` scope returns the URL in the OAuth response — so nothing is
copied by hand. `craft watch` then needs no `--webhook` at all.

One scope, and the narrowest one that works: permission to post to the single
channel you pick. Not `chat:write`, which would be permission to post anywhere
in the workspace.

The webhook URL comes from `--webhook`, then `ANACRAFT_WEBHOOK`, then whatever
`craft slack --install` saved in `~/.anacraft/slack.json` — and deliberately
**not** from `config.toml`. That file is meant to be safe to commit to a
dotfile repo, and a URL that can post into your Slack is not.

`--webhook` stays for cron, CI, and workspaces where you cannot install apps.

`craft watch` to the terminal is part of the Anacrafter plan; delivering it to
Slack is what Anacrafter Pro adds (the same command, a `--webhook`).
`craft watch --demo` needs neither, so what an alert looks like can be seen
before anything is paid for or wired up.

## The dashboard

Seven panels, each toggleable. Turn off what you do not care about and the rest
reflows to fill the terminal.

| Key | Panel | What it shows |
|-----|-------|---------------|
| `1` | **EVENTS** | Event count per day, this period drawn over the last one, with the total and its change |
| `2` | **RIGHT NOW** | Live player count plus a spawn / wander-off event feed |
| `3` | **COUNTRIES** | Traffic plotted on a world map |
| `4` | **TOP PAGES** | Most-visited pages with view bars and rank movement |
| `5` | **VITALS** | Users, sessions, views, conversions, bounce rate, avg. session |
| `6` | **TOP COUNTRIES** | Ranked countries with tier markers |
| `7` | **DAILY USERS** | User trend across the period |

### Controls

| Key | Action |
|-----|--------|
| `1`–`7` | Toggle a panel — `e` `l` `m` `p` `v` `g` `d` do the same |
| `tab` | Next property, when more than one is configured |
| `shift`+`D` | Forget the property on screen — drops it from the rotation, leaves it in Google |
| `t` | Cycle the palette, and save it |
| `s` | Demo only — preview the Anacrafter look |
| `r` | Rebuild — force a refetch now |
| `?` / `h` | Help overlay |
| `q` / `Esc` | Quit |

## Palettes

```sh
craft theme                # list the palettes with swatches
craft theme tokyo-night    # switch and persist
craft --theme github dash  # override for one run
```

`osaka-jade` (default) · `solarized-dark` · `tokyo-night` · `catppuccin` · `github` · `solarized-light` · `catppuccin-latte`

The ore vocabulary — diamond, gold, redstone, lapis — is mapped onto whichever
palette is selected, so the texture pack survives a theme swap.

The command is `craft`. `anacraft` is installed alongside it as an alias, so
older scripts and anything you have in muscle memory keep working.

## Ask an assistant

`craft mcp` serves the dashboard's numbers over the [Model Context
Protocol](https://modelcontextprotocol.io), so Claude Desktop, Claude Code, or
any MCP client can answer "how is the site doing" without a human reading a TUI.

```sh
craft mcp --install           # write Claude Desktop and Smartloop configs
craft mcp --install --demo    # use synthetic data in both entries
craft mcp --uninstall         # take the server back out of Claude Desktop's config
craft mcp                    # the server itself; clients spawn this, you rarely do
craft mcp --demo             # synthetic data, no Google account, no subscription
```

Claude Desktop is [one command](#claude-desktop). For Claude Code it is
`claude mcp add anacraft -- craft mcp`; any other client takes the same command
and argument. Writing a config by hand, use an **absolute path** — a desktop app
is not launched from a shell and does not inherit the `PATH` where `craft`
works. `which craft` gives the value to paste.

| Tool | Answers |
|------|---------|
| `site_status` | Headline metrics against the period before, the daily user series, and the achievements that fired |
| `audit_site` | Whether the property is measuring correctly: twelve graded checks over 28 days, each carrying what it means |
| `live_visitors` | Who is on the site right now, by country |
| `list_pages` | Most-visited pages |
| `list_events` | Events by count, with the per-day total against the previous period |
| `list_referrers` | The URLs sending traffic |
| `list_traffic_sources` | GA4 source / medium pairs |
| `list_countries` | Traffic by country |
| `list_properties` | Every property this account can read |
| `search_pages` | Pages whose path contains a substring |
| `search_events` | Events whose name contains a substring |
| `configure_site` | Creates the property and web stream for a domain and returns the tag to paste — the one write |

Every report tool takes an optional `property` and falls back to the saved
default, so an assistant that knows nothing about your config still gets
answers. Each also carries its own `days` default rather than sharing one,
because the window a question needs is part of the question — seven days is the
right answer to "how are we doing" and the wrong one to "is anything broken",
so `audit_site` advertises twenty-eight. `configure_site`, the one writer, takes a `domain` instead — the point
is to create the property. Responses are structured JSON — labelled numbers
carrying the property id and the date window they cover, not rendered panels.

**One writer.** `configure_site` creates a property and web stream for a domain
the account doesn't track yet and returns the tag; it works off the stored
grant rather than opening a browser. Nothing else here starts an OAuth flow,
writes to `~/.anacraft/`, or changes the default property: `login` and `use`
stay human-only commands, and `configure_site` never sets the default either
(say `craft use` for that). If no
credentials are stored the tools say to run `craft login` rather than opening a
browser inside your client's subprocess. Identical reports
are cached for a minute so a chatty agent does not burn the GA4 quota that the
dashboard needs.

**Subscription.** `craft mcp` is the Anacrafter **Elite** plan — `craft subscribe`
for the $2.99 starter, `--plan pro` / `--plan elite` for the two above it, and
[anacraft.dev/pricing](https://anacraft.dev/pricing.html) for what is on each
side of that line. It opens Stripe,
waits for the payment to clear, and writes `supporter = true` (and the plan)
itself; the
dashboard, `craft watch` and the MCP server re-check on launch and keep that
line current. The
record is keyed to the Google account you signed in with, so a second machine
only has to `craft login` — add `--check` to look it up without opening a
browser. Missing it does not take the process
down: an MCP client reads an early exit as "server disconnected", which says
nothing about what to fix, so the server starts, the handshake succeeds, and
every tool call answers with the sentence that gets you unstuck. The same goes
for a missing login. `craft mcp --demo` is ungated, so the server can be wired
up and looked at first.

## Configuration

| File | Purpose |
|------|---------|
| `~/.config/anacraft/config.toml` | Properties and their settings |
| `~/.anacraft/token.json` | OAuth refresh token, written by `login` |

Config honours `$XDG_CONFIG_HOME`. Tokens stay out of `~/.config` on purpose —
that directory ends up in dotfile repos, and a refresh token has no business
travelling with it. A pre-0.4 `~/.anacraft/config.json` is migrated on first run.

### Multiple properties

`craft use <id>` adds a property rather than replacing the last one, so the
config accumulates. In the dashboard, `tab` cycles between them, and whichever
one you quit on becomes `active` — so the dashboard and the rest of the CLI do
not disagree about which property is the current one. Passing through on the
way somewhere else costs nothing; landing is what commits it.

`active` is what every command reads when you do not say otherwise. The order
is `--property <id>`, then `ANACRAFT_PROPERTY_ID`, then `active`, so a flag or
an exported id will quietly outrank `craft use` for as long as it is set.

```toml
active = "397412345"
theme  = "osaka-jade"        # palette for any property that doesn't name one

[[property]]
id           = "397412345"
name         = "anacraft.dev"
label        = "site"        # shown instead of name in the switcher
theme        = "catppuccin"
days         = 14
refresh      = 60
live_refresh = 5

[[property]]
id = "88820011"              # everything optional: inherits the defaults
```

Every key under `[[property]]` is optional and falls back to the global default,
so switching to a property that saved nothing lands on the defaults rather than
inheriting the previous property's window. Command-line flags beat both.

`ANACRAFT_PROPERTY_ID` overrides the saved property if you would rather not keep
one on disk.

### Your own OAuth client

Official builds carry one, so `craft login` works with no setup. To use your own
instead, set `ANACRAFT_OAUTH_CLIENT_ID` and `ANACRAFT_OAUTH_CLIENT_SECRET`, or
write `~/.anacraft/client.json`. Both take precedence over the built-in client,
and registering your own Google Cloud project also insulates you from other
people's quota consumption.

## Setting up GA4

`craft configure <domain>` is the preferred route: property, data stream and
tag in one command, without the console. It is part of the subscription, the
same as `craft watch` and `craft mcp` — the ask arrives on the page the Google
sign-in already ends on, and nothing is created in the Analytics account until
the payment clears. The rest of the Google side — access management, retention,
key events, API enablement — is console work, and is documented in
[Configure your analytics](https://anacraft.dev/setup-ga4.html).

`craft delete <domain|id>` is the way back out, and on its own it does less
than it sounds like: it forgets the property here so the dashboard stops
opening on it, then prints the console link and the two clicks that delete it.
Nothing in Analytics changes.

`craft delete <domain|id> --all` does those two clicks for you. It is the only
command that deletes anything in Google, it only ever touches the property you
named, and it has to be typed — a bare `craft delete` will never do it. What it
reaches for is Google's own soft delete, so the property lands in your
Analytics account's trash and stays restorable from the console for 35 days
before it and its data are gone for good.

Cloned the repo and use [Claude Code](https://claude.com/claude-code)?
`.claude/skills/anacraft/` ships as a skill — installing, connecting a
property, driving the dashboard, wiring up `craft mcp`, and what each error
message actually means. Ask Claude to set anacraft up, or paste a failing
command at it.

## Requirements

- A Google Analytics 4 property
- A terminal with truecolor support
- Rust 1.74+, if you are building from source

## Contributing

`cargo run -- dash --demo` gets you a working dashboard with no Google account
attached. See [CONTRIBUTING.md](CONTRIBUTING.md) for the layout of the code and
what CI expects.

## License

[Apache License 2.0](LICENSE)
