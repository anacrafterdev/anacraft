# When `craft` fails

Errors print to stderr as `⛏ <message>`, with the cause chain dimmed under it —
that chain is where Google's own wording lives, so read it before deciding what
went wrong. Almost every message names the command that fixes it.

## By message

| Message | Cause | Fix |
|---|---|---|
| `no property selected — run craft props to pick one` | Nothing saved, and no `--property` or `ANACRAFT_PROPERTY_ID` | `craft props`, then `craft use <id>` |
| `no property <id> on this account — run craft props` | Wrong id, or the account cannot see that property | Read the id off `craft props`. If the property is missing from that list entirely, it is an access problem — see below |
| `not logged in — run craft login` | No `~/.anacraft/token.json` | `craft login` |
| `login expired — run craft login` | The refresh token was revoked, or the password changed | `craft login` again |
| `access denied — the signed-in account needs at least Viewer on this property` | Real permission gap, in GA4 | Grant the account access: [Configure GA4](https://anacraft.dev/setup-ga4.html) |
| `an API isn't enabled on your Google Cloud project` | Data API or Admin API off | Enable **both**; one being on is the usual half-fix |
| `Google rate-limited this request; try again shortly` | GA4 quota, shared with anything else reading the property | Wait. If it repeats, lengthen `refresh`, or register your own OAuth client so the quota is yours |
| `unexpected response shape from Google` | An API returned something unparseable — nearly always an error page behind a proxy or captive portal | Check the network before the tool |
| `no theme called <name>` | Typo | `craft theme` lists them |
| `craft mcp is on Anacrafter Elite` | The gate. Returned by every tool call, and printed to the client's log at startup — the server keeps running | `craft subscribe --plan elite`, then `supporter = true` and `tier = "elite"` in the config. Or `craft mcp --demo` |
| `Slack alerts are on Anacrafter Pro` | The gate around a `craft watch --webhook` run on a plan below Pro | `craft subscribe --plan pro`, or drop `--webhook` and read the watch in the terminal |
| `not logged in — run craft login in a terminal, then restart the MCP client` | The MCP server found no token | Run `craft login` yourself, in a terminal — the server will not open a browser |
| `found nowhere writable to install to` | Installer could not write to `/usr/local/bin` or `~/.local/bin` | `INSTALL_DIR=~/bin curl … \| bash` |
| `could not work out the latest release` | GitHub API unreachable or rate-limited | `VERSION=v0.6.0 curl … \| bash` to pin one |

## Symptoms with no error

**"No data in this window."** The property is reachable and genuinely returned
nothing. Check the days: the window ends **yesterday**, because today is still
partial, so a site that started collecting this morning has nothing to show.
Then confirm data is arriving at all — GA4 Realtime, not its standard reports,
which lag 24–48 hours.

**The numbers arrive but look wrong.** Conversions stuck at zero, revenue
always nothing, a bounce rate too good to be true, traffic that is almost all
direct. Run `craft audit` before investigating any of them individually: all
four have a common measurement cause, the audit names which, and each finding
carries what it means and what fixes it. `craft audit --demo` shows the shape of
the report without touching an account.

**Realtime says zero but the reports are fine.** `live` is a 30-minute window.
Zero is usually the truth about a small site.

**The dashboard shows numbers with an error line.** A refresh failed and the
last good numbers stayed on screen rather than blanking the layout. If the
error is a rate limit it will clear itself; `r` forces a retry.

**The property is missing from `craft props`.** That list is what the
signed-in Google account can read. Either the wrong account was used at
`craft login` (`craft logout`, then `craft login`), or nobody has granted it
access to the property yet.

**A `G-XXXXXXXXXX` id will not work.** That is the measurement id, which
belongs to the tag. `craft` wants the numeric property id — `397412345`, from
GA4's Admin → Property details, and printed next to every name in
`craft props`. The two are not interchangeable, and this accounts for most
"my property isn't found" reports.

**Colors look flat or wrong.** The palettes are truecolor. A terminal in
256-color mode approximates them badly; check `$COLORTERM` is `truecolor` or
`24bit`.

**A resize notice instead of the dashboard.** Below 80×24 it says so rather
than drawing something broken. The notice prints the current size against the
minimum.

**`Ctrl`+digit does nothing.** Not every terminal can send it. The bare digits
`1`–`7` and the letters `e l m p v g d` toggle the same panels.

**Nothing happens on `tab`.** It walks the configured properties, so a config
with one property has nowhere to go. `craft use <id>` adds another.

## Where the problem is not

If the fix is in the GA4 console — no data arriving, a missing tag, access
management, API enablement, key events, retention — this tool can only report
what Google will hand it. Two of those anacraft can now fix directly:
`craft configure <domain>` creates a missing property and prints the tag to
paste, which covers most of "no data arriving". The rest has its click paths
written down in
[Configure GA4](https://anacraft.dev/setup-ga4.html).
