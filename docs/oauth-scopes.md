# OAuth scopes anacraft asks for

Every permission this binary requests, what uses it, and the justification for
the one that is new in 0.12. The wording under [Justification](#justification)
is written to be pasted into the Cloud Console verification form.

## The set

| Scope | Tier | Asked at | Used by |
| --- | --- | --- | --- |
| `openid` | Non-sensitive | `craft login` | Keying a subscription to an account so it survives a new laptop |
| `email` | Non-sensitive | `craft login` | Naming the account in support questions |
| `.../auth/analytics.readonly` | Sensitive | `craft login` | Every report, the dashboard, `craft watch`, `craft mcp`, and the `accounts.list` / `properties.list` / `dataStreams.list` reads `craft configure` does before it creates anything |
| `.../auth/analytics.edit` | Sensitive | `craft login`, with the read scope | `properties.create`, `dataStreams.create` and `properties.delete` on the property the user names, plus `keyEvents.create` and an update-masked `enhancedMeasurementSettings.patch` behind `craft audit --fix` — nothing else. Credentials granted before the edit scope joined the login set are topped up the first time a write command runs after that (`ensure_scope`) |

Only one scope is being added: `analytics.edit`. The reads `craft configure`
performs — finding the account, and checking whether the domain already has a
property — are all covered by the read-only scope the app already holds.

Cloud Console labels each scope's tier on the consent screen configuration
page; confirm the label there when adding the scope, since Google does not
publish a per-scope list. `analytics.edit` sits in the same tier as the
read-only Analytics scope already in use, so this is a re-verification of an
app that is already verified for sensitive scopes — not a first submission, and
not a move into the restricted tier that would bring a third-party security
assessment with it.

## Why the scope's write half is used

Two commands, and no others.

### `craft configure`

`craft configure example.com` does, as one command, what the Analytics console
asks a person to do across four screens: create a GA4 property, add a web data
stream for the domain, and read back the measurement id. It then prints the
gtag.js snippet with that id already in both of the places it belongs. It is
step 01 of [Configure your analytics](setup-ga4.html), which no longer documents the
console route it replaced.

That is the whole feature. It is also the most-read page on the site, which is
why it is worth a command.

Two Admin API calls do the work, and Google documents the same requirement for
both:

| Call | Documented scope |
| --- | --- |
| [`properties.create`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/properties/create) | `https://www.googleapis.com/auth/analytics.edit` |
| [`properties.dataStreams.create`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/properties.dataStreams/create) | `https://www.googleapis.com/auth/analytics.edit` |

### `craft serve`

The same two calls as `craft configure`, made for a page in a browser instead
of a terminal. `craft serve` runs an HTTP API on `127.0.0.1` and opens a page
on it where somebody signs in, picks or creates a property, and copies the tag;
`POST /v1/properties` is `craft configure` underneath, calling the same
`configure::setup` and so making `properties.create` and `dataStreams.create`.
`DELETE /v1/properties/{id}` is `craft delete --all` underneath, making
`properties.delete` — Google's soft delete, into a trash the console restores
from for 35 days — and it is opt-in twice the way the command is: the id in the
path, and the same id again in `?confirm=`.

It adds no scope and no call. What it adds is a second way to reach the three
that are already here, which is why it is named: the surface a reviewer can run
is not the same as the surface a reviewer is told about unless both are
written down. Every endpoint it serves is at
[anacraft.dev/serve.html](https://anacraft.dev/serve.html).

### `craft audit --fix`

`craft audit` reads a property and reports what is wrong with how it measures:
events configured as outcomes that never fire, page views counted twice,
payment pages credited with conversions. It is a read-only command and it stays
one.

Most of what it finds cannot be fixed from an API at all — an event the site is
not sending cannot be made to arrive by changing a setting. Two things can, and
`--fix` is the flag that does them:

| Call | What it does | Documented scope |
| --- | --- | --- |
| [`properties.keyEvents.create`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/properties.keyEvents/create) | Marks an event the property is **already recording** as an outcome, so conversion reports count it | `https://www.googleapis.com/auth/analytics.edit` |
| [`properties.dataStreams.updateEnhancedMeasurementSettings`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1alpha/properties.dataStreams/updateEnhancedMeasurementSettings) | Turns on automatic measurement the tag on the site already supports — scrolls, outbound clicks, video engagement, file downloads | `https://www.googleapis.com/auth/analytics.edit` |

Neither writes data, changes history, or alters what the site sends. Both are
undone from the Analytics console in one click — key events under Admin →
Events, measurement under Admin → Data Streams → Enhanced measurement.

Two of GA4's enhanced measurement toggles are deliberately **not** in that
second row. Site search and form interactions record what a visitor typed — the
query, and which fields of a form were touched — and whether a site collects
that is a question about its privacy policy rather than about whether its
analytics are configured correctly. A person running `--fix` to mark a key
event has not agreed to start collecting typed input. Both are still reported
by the audit, with the plan line naming them as staying off; neither is ever
written. (`src/audit.rs`, `MEASURED`; pinned by
`the_fix_never_switches_on_collection_of_what_a_visitor_typed`.)

The second is the only call in this app against the Admin API's `v1alpha`
surface, because enhanced measurement has never been promoted to `v1beta`.
Everything read from it is optional: a check that cannot reach it is reported
as not run rather than as a pass.

## Justification

**What the app does.** anacraft is a terminal dashboard for Google Analytics 4.
It reads a property's reports and draws them in a terminal. It runs entirely on
the user's own machine, as a single binary, against the user's own Analytics
account. There is no anacraft server that reports data passes through.

**Why `analytics.edit` is necessary.** One command, `craft configure <domain>`,
sets a website up in GA4 so the dashboard has something to read: it creates a
property, creates that property's web data stream, and prints the resulting
measurement id as a copy-and-paste gtag.js snippet. `properties.create` and
`properties.dataStreams.create` are the only ways to do that, and both document
`analytics.edit` as their required scope. Without it, a first-time user has to
leave the tool, complete a multi-screen setup in the Analytics console, and copy
a measurement id back by hand — which is the step where they currently stop.

**Why a narrower scope will not work.** The Google Analytics Admin API v1beta,
which is the API this feature calls, publishes exactly two scopes:
`analytics.readonly` and `analytics.edit`. There is no third, no per-method
scope, and no create-only scope. `properties.create` and
`dataStreams.create` accept only `analytics.edit`. So this is not a case of a
narrower scope existing and being passed over — for this API the choice is two
scopes wide, and the other one cannot create.

Every other Analytics scope Google publishes belongs to the older Analytics API
v3 and grants strictly more than this feature uses. For completeness, with
Google's own descriptions:

| Rejected alternative | Google's description | Why it is worse |
| --- | --- | --- |
| `.../auth/analytics` | "View and manage your Google Analytics data" | Edit plus report data. The app already reads via `analytics.readonly`; this would request reads a second time |
| `.../auth/analytics.manage.users` | "Manage Google Analytics Account users by email address" | Adds adding and removing people, and changing their permissions. Nothing here touches who can see an account |
| `.../auth/analytics.manage.users.readonly` | "View Google Analytics user permissions" | Reads the permission list. Nothing here needs it |
| `.../auth/analytics.provision` | "Create a new Google Analytics account along with its default property and view" | Belongs to the v3 provisioning API. The GA4 equivalent, `accounts.provisionAccountTicket`, sits in Admin API v1beta under `analytics.edit`, so this adds nothing this app could use |
| `.../auth/analytics.user.deletion` | "Manage Google Analytics user deletion requests" | Deletes end-user data. Nothing here does |

**What `analytics.edit` grants that this app does not use.** Stated plainly,
because it is the honest shape of the request: "Edit Google Analytics management
entities" covers updating and deleting configuration, not only creating it. The
scope is broader than the feature, and no narrower one exists to drop to. What
narrows the grant is therefore the code rather than the scope — which is what
the four measures below are, and why the second of them fails the build rather
than merely documenting an intention.

**How the request is minimised.** Four things, all verifiable in the source:

1. **It is requested once, at sign-in, on the same consent screen as everything
   else.** `craft login` asks for the full set — read and edit together — in a
   single browser trip, so `craft configure` never has to interrupt the setup
   guide with a second screen. The write scope is not hidden from the review: it
   sits on the same screen as the read scope, and `configure` names the line
   what it is for. Credentials granted before the edit scope joined the login
   set are topped up through Google's incremental authorization the first time a
   write command runs after that (`src/auth.rs`, `ensure_scope`). Re-running
   `craft configure` on a domain that is already set up therefore completes
   entirely within access it already holds and shows no consent screen.
   (Pinned by the tests `signing_in_asks_for_analytics_and_nothing_else` and
   `the_write_scope_is_the_narrowest_one_that_creates_a_property`.)
2. **It modifies two named settings, additively, and deletes only what the
   user names.** The grant permits updating any property, stream, setting or
   user. The client reaches two, both behind `craft audit --fix`, and both in
   the direction of collecting more rather than less:

   - `keyEvents.create` marks an event the property is already recording as an
     outcome. It is a create, not an update; it changes no existing resource,
     and the events it can mark are a literal list in the source
     (`src/audit.rs`, `OUTCOMES`) rather than anything inferred at runtime.
   - `enhancedMeasurementSettings.patch` is the only `PATCH` in the client. It
     is sent with an `updateMask` naming the exact fields to change, so it
     cannot reach a setting the command did not print first, and the function
     that builds it has no argument that can express `false` — it sets
     measurement toggles on and has no way to turn one off. The worst outcome
     of a defect in it is an event being collected that nobody asked for, which
     the console undoes in one click. (Pinned by
     `nothing_the_fix_path_writes_can_turn_collection_off`.)

   There is no `PUT` anywhere in the client: a whole-resource replace is how a
   write meant to change one setting silently changes four, and nothing here
   has a reason to want that.

   Both are reached only from `craft audit --fix` — never from `craft audit`,
   which stays a read-only command. Every change is printed as a plan under the
   finding that motivates it before the flag is passed, and printed again, one
   line per write, as it is made.

   It issues exactly one `DELETE`: `properties.delete`, against the single
   property named on the command line, reached only from `craft delete --all`.
   Nothing calls it in a loop, nothing infers a target, and the bare `craft
   delete` — the command without the flag — still changes nothing in Analytics
   at all. It forgets the property in anacraft's own config so the dashboard
   stops opening on it, then prints the console's delete path. Deleting in
   Google is a second, explicit thing to type.

   The flag exists because the asymmetry was the strange part: `craft configure`
   creates a property in one line, and undoing that took four console screens.
   What keeps it safe is not a confirmation prompt — `--all` *is* the
   confirmation, and a prompt behind an explicit flag only trains people to
   press `y`. It is that Google's delete is a soft one. The property goes to
   the Analytics account's trash and stays restorable there for 35 days, the
   account's own permission checks still apply, and the command prints that
   window before it prints anything else. The undo is Google's and no code here
   can shorten it. (`src/configure.rs`, `delete`; `src/ga.rs`,
   `delete_property`; pinned by
   `the_admin_api_surface_is_the_one_documented_in_the_scope_submission`, which
   fails the build if a request appears that this page does not describe.)
3. **It will not create twice.** Before creating anything, `craft configure`
   looks for a property that already measures the domain and reuses it,
   printing its existing tag. Re-running the command is the supported way to
   get the tag back, and does not leave a second property behind.
   (`src/configure.rs`, `find_existing`.)
4. **It does not create accounts, though the grant would permit it.**
   [`accounts.provisionAccountTicket`](https://developers.google.com/analytics/devguides/config/admin/v1/rest/v1beta/accounts/provisionAccountTicket)
   creates an Analytics account and it requires `analytics.edit` — the same
   scope, no wider one. This app does not call it. If the signed-in account has
   no Analytics account, `craft configure` stops and links the user to the
   console instead, because creating an account means accepting Google's terms
   and that is a decision to make in Google's own words, on Google's own page.
   This is the pattern: the grant is one scope wide, and what the app does with
   it is narrower than what the scope allows — the writes are five named
   methods against resources the user typed, four of them additive, and the one
   destructive method is behind a flag. (`src/configure.rs`, `pick_account`.)

**Where the data goes.** Nowhere. Tokens are written to `~/.anacraft/token.json`
at mode `0600` on the user's own machine, alongside no copy of any report.
Analytics data is rendered to the terminal and is not transmitted to anacraft or
to any third party. The only network calls are to Google's own APIs, plus — if
the user configures them — a Slack webhook they supply and a subscription
lookup that sees an account id and no analytics data.

## Demo video script

For the verification submission, recording the whole flow end to end:

1. `craft login` — show the consent screen. Read the scopes out loud: read-only
   Analytics access for the reports, and edit access for the one command that
   sets a site up (`craft configure`).
2. `craft` — the dashboard, reading the account's numbers. This is the product,
   and it works entirely within the read scope.
3. `craft configure example.com` — the property and stream are created with the
   permission already granted at login; no second consent screen appears. (For
   an account whose credentials predate the edit scope, this is where the
   one-time incremental grant is asked for, with the terminal line naming what
   it is for.)
4. Show the property and the stream being created, and the printed tag.
5. Open the Analytics console and show the new property and its data stream,
   matching what the terminal printed.
6. `craft configure example.com` again — show that it finds the existing
   property, creates nothing, prints the same tag, and asks for no permission
   in the process.
7. `craft delete example.com` — show that the command whose name implies
   deletion does not, on its own, use the grant to delete. It forgets the
   property locally and prints the console path, leaving the property intact in
   Analytics. This is the step that shows the grant is bounded by the code
   rather than by the scope.
8. `craft audit` — the read-only report. Show a finding that offers a fix, the
   `fix ·` line under it naming exactly what would change, and that running the
   command has changed nothing in Analytics.
9. `craft audit --fix` — the opt-in. Show each write printed as it is made,
   then the Analytics console with the key event now marked and the measurement
   toggle now on. Then show both being undone in the console in a click, which
   is the point: the command is a shortcut through settings the user could have
   changed by hand, not a capability they did not otherwise have.
10. `craft delete example.com --all` — the opt-in. Show the property moving to
   the Analytics trash, the terminal naming the 35-day restore window, and then
   the console with the property sitting in Admin → Account → Trash, restorable.
   One property, the one named on the command line.
