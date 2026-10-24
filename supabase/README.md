# The service side

Two RPCs, one webhook, and one relay. Everything else about anacraft runs on the user's own
machine; this exists only so a payment made in a browser can be seen by a binary
on a laptop — and so it is still seen after that laptop is replaced.

```
craft subscribe                  Stripe                    craft dash / craft mcp
     │                              │                                │
     ├─ claim_checkout(token, ──────┤                                │
     │  google account id)          │                                │
     ├─ opens the Payment Link ────►│                                │
     │  ?client_reference_id=token  │                                │
     │                              ├─ checkout.session.completed ──►│ (webhook)
     │◄─ subscription_status ───────┴────────────────────────────────┤
     │   polls until active                        subscription_status
     ▼                                             on every launch
  supporter = true
```

The row is keyed on the Google account id (`sub`), which is why a new machine
only has to `craft login` with the same account. The token is kept as a second
key so a checkout that completed before the identity landed still resolves.

## The other way in

`pricing.html` has a Subscribe button that goes straight to the same Payment
Link, with nothing in front of it. Those checkouts arrive with no
`client_reference_id` and no account:

```
pricing.html ──► Stripe ──► checkout.session.completed
                                    │
                                    ├─ no client_reference_id, so the webhook
                                    │  mints a `web_…` token and writes the row
                                    │  with user_id null
                                    ▼
                             craft login ──► link_account(sub, email)
                                    │        adopts any unowned row whose
                                    ▼        email matches, once and one way
                             supporter = true
```

Adoption is `link_account` in the users migration, which only ever takes a row
with `user_id is null` — an email is a key to an unclaimed payment, never a way
to *move* somebody else's. `craft subscribe` calls it too, before its own lookup,
so a machine that was already signed in when the payment happened picks it up
on `craft subscribe --check` without signing in again.

Reading is the other half of that key, and since the `email_answers_too`
migration `subscription_status` takes the address as a third argument. It has to:
`user_id` is Google's `sub`, which is per account rather than per person, so a
payment made under one Google account and a sign-in under another leaves the
subscription attached to an id nothing ever asks about again — and the person
holding an active subscription gets asked to buy a second one. The address
answers where the id cannot. It still moves nothing: the payment stays on
whatever row it was on, and the only thing an address can turn is a no into a
yes.

A subscriber Stripe has never heard of — whoever wrote this, a lifetime handed
out for helping, an apology — is `users.comped`, added by the
`a_grant_that_survives` migration. It is the one column on that table meant to
be written by hand, and it exists because `subscribed` is not: that one is
derived, and `refresh_subscribed` recomputes it from the payments table on
every sign-in and every claim, so a `subscribed = true` typed in by hand is
erased by the next command that touches the service. `subscribed` is now
`comped or exists(active payment)`, and nothing that reads it had to change.

```sql
update public.users set comped = true where lower(email) = 'them@example.com';
select public.refresh_subscribed(user_id) from public.users;  -- settle the flag
```

Somebody who pays with an email that is not on their Google account is still
the one case with no automatic path — neither key matches, by design. The row
is there with its Stripe customer on it; setting its `user_id` by hand is the
fix, and `craft subscribe --check` now names the address it asked about so that
case is recognisable rather than mysterious.

## Maker access

`users.maker`, added by the `maker` migration, is Elite granted by hand and not
by Stripe — currently, to anybody with push access to the anacraft repo.
Where `comped` only ever grants `subscribed`, `maker` reaches `tier` too:
`subscription_status` reads a maker row as `elite` even with no payment row
behind it at all, because the point of the flag is "every gate opens", not
"treat this like a $9.99 subscription".

It is the one column on `users` nothing writes by hand — `sync_makers` is the
only path, and it is a full sync rather than a grant: it takes the current
list of collaborator emails and settles every row to match it, so leaving the
repo revokes the flag the same run that joining it would have granted it.
`.github/workflows/ci.yml` calls it (via `scripts/sync-maker.py`) on every push
to `main`, matched against whatever email each collaborator has made public on
their GitHub profile — a private email, or an account that has never run
`craft login`, is left untouched. It authenticates with the project's URL and
service-role key, held as the `SUPABASE_URL` and `SUPABASE_SERVICE_ROLE_KEY`
repo secrets — the same service key the webhook and `burn` function use.

## What the binary carries

The publishable key, which is public by design. The table has RLS on and no
policies, so that key reaches nothing directly — only `claim_checkout` and
`subscription_status`, both `security definer`, both answering about a single
token, account or address. No listing, no customer ids, no email *out*: an
address has to be supplied to be asked about, and what comes back is a
yes/no, a status word, a date and a founder number for an address the caller
already had. Weighed against the alternative, which was subscribers being
asked to pay twice, that is the trade the `email_answers_too` migration makes
and argues for at length.

## Deploy

```bash
supabase link --project-ref <ref>
supabase db push                                   # the migrations
supabase secrets set STRIPE_SECRET_KEY=sk_live_... STRIPE_WEBHOOK_SECRET=whsec_...
supabase functions deploy stripe-webhook --no-verify-jwt
```

`--no-verify-jwt` is required: Stripe does not send a Supabase token. The
signature check inside the function is what authenticates the call.

Check what the deployment can see — booleans only, no values:

```bash
curl -s https://<ref>.supabase.co/functions/v1/stripe-webhook/health
{"stripe_secret_key":true,"stripe_webhook_secret":true,"service_key":true,"supabase_url":true}
```

The two Stripe ones are the secrets above; the other two are injected by the
platform. Note the service key arrives as `SB_SECRET_KEY` on a project using
the new API keys and as `SUPABASE_SERVICE_ROLE_KEY` on one still using the
legacy JWT keys — the function reads both.

In the Stripe dashboard, add the endpoint

```
https://<ref>.supabase.co/functions/v1/stripe-webhook
```

subscribed to

| Event | What it writes |
|---|---|
| `checkout.session.completed` | The payment landed — fills in the claimed row |
| `checkout.session.expired` | The checkout was abandoned — settles a row stuck at `pending` |
| `customer.subscription.created` | |
| `customer.subscription.updated` | Status, period end, and the price being charged |
| `customer.subscription.deleted` | |
| `customer.subscription.paused` | |
| `customer.subscription.resumed` | |
| `invoice.paid` | The amount that actually cleared on a renewal |
| `invoice.payment_failed` | Same, for an attempt that did not — status is left to `subscription.updated` |

Copy the endpoint's signing secret into `STRIPE_WEBHOOK_SECRET` above.

Everything else Stripe sends is answered `200` and ignored, so subscribing to
more than this list is harmless — but the function only acts on these.

`expired` is the one status word here that is not Stripe's. It means a checkout
that was claimed and never paid, and the binary reads it the way it reads
`pending`: as no evidence either way, so it never clears a `supporter` flag
somebody set by hand. A cancellation says `canceled`, and that is an answer.

## What the binary is built with

```bash
ANACRAFT_SUPABASE_URL=https://<ref>.supabase.co \
ANACRAFT_SUPABASE_KEY=<publishable key> \
cargo build --release
```

Both are `option_env!` and neither is written into the source. The publishable
key is safe to *send* — it is in every request the binary makes, and the table
is closed to it — but that is not the same as safe to commit: a key in git
outlives its rotation. The release workflow reads them from repository secrets
of the same name.

Both are read from the environment at runtime too, which is how a debug build
points at a local `supabase start`:

```bash
ANACRAFT_SUPABASE_URL=http://127.0.0.1:54321 \
ANACRAFT_SUPABASE_KEY=<publishable key> \
cargo run -- subscribe --check
```

A build with neither has no lookup at all, which is still a working build:
`craft subscribe` opens Stripe and falls back to saying which line of config to
set.

## Checking it by hand

```bash
curl -s "$URL/rest/v1/rpc/subscription_status" \
  -H "apikey: $KEY" -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"p_user_id":null,"p_token":"<token from ~/.anacraft/license.json>"}'
```

Any of the three keys will do, and the address is the one to reach for when
somebody says they have paid and the binary disagrees. An empty answer (`[]`)
means no row is attached to any of them; a row with `"subscribed": true` and a
`user_id` that is not theirs is the account-id mismatch this all exists for.

```bash
curl -s "$URL/rest/v1/rpc/subscription_status" \
  -H "apikey: $KEY" -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"p_user_id":null,"p_token":null,"p_email":"them@example.com"}'
```

## The badge endpoint

`burn` serves the image `craft burn` hands out, as an SVG, from a row the CLI
publishes. It is the only function here that renders anything for a stranger's
browser, and the only one on the critical path of a page that is not ours — so
it answers something on every request (an unknown id gets a plain `anacraft`
pill, never a broken image or a zero somebody might believe) and caches for
five minutes at a number that moves on the order of days.

It holds no credentials and can reach no Analytics. The count was worked out on
the site owner's own machine and published through `publish_badge`, which takes
a secret minted alongside the badge id; the anon key cannot be the check,
because it ships inside a binary anybody can download.

Nor is there a schedule on this side that recounts, and there cannot be: only a
machine with the owner's credentials can count. `craft burn` mints the badge
once, and `burn::keep_current` republishes the number from the dashboard,
`craft watch` and the MCP server as they run — twice a day at most, against a
thirty-day window.
A cron here would mean holding standing access to somebody's Analytics, which
is the one thing this whole arrangement exists to avoid. Colours travel in the
row rather than living in this function, so a palette added to the CLI works
the day it ships instead of the day this is redeployed.

```bash
supabase functions deploy burn --no-verify-jwt
```

`--no-verify-jwt` for the same reason as the relay below: an `<img>` sends no
Supabase token and could not be made to.

## The Slack relay

`slack-oauth` has nothing to do with subscriptions and holds no secret. It
exists because Slack refuses a loopback redirect URL on a publicly distributed
app — every URL on one must be HTTPS — while `craft slack --install` is a
process on a laptop listening on an ephemeral port.

```
craft slack --install            Slack                      slack-oauth
     │                             │                             │
     ├─ PKCE verifier, port 0      │                             │
     ├─ opens authorize ──────────►│                             │
     │   ?state=<nonce>.<port>     │  user picks channel         │
     │                             ├─ redirect ?code&state ─────►│
     │◄──────── 302 to 127.0.0.1:<port>?code&state ──────────────┤
     ├─ oauth.v2.access with code_verifier, no secret ──►│
     ▼
  ~/.anacraft/slack.json (0600)
```

The port travels inside `state`, which Slack echoes back verbatim and the CLI
checks in full — so carrying a second value there costs none of its purpose as
a CSRF token. The relay reads the port as the last dot-separated field and
forwards only to `127.0.0.1`, because a relay that forwarded anywhere would be
an open redirect wearing our domain.

A code passing through here is useless to whoever sees it, this function
included: PKCE means the exchange needs the verifier, which never leaves the
CLI. That is what lets the relay be public and unauthenticated.

```
supabase functions deploy slack-oauth --no-verify-jwt
```

No secrets to set. The function's URL goes in the Slack app's **Redirect URLs**
and in `ANACRAFT_SLACK_REDIRECT` at build time.

## The web app

`anacraft.dev/app` is a static site on GitHub Pages (`docs/app/`) with no
server of its own. Google signs the visitor in with its flow for browser apps
and hands the access token straight to the page, which calls the Admin API
with it. The page calls `link_account`, `subscription_status` and
`claim_checkout` with the publishable key, exactly as the CLI does, so there
is nothing here to deploy for it.

The Pages workflow writes the page's `config.js` from the repository secrets
`ANACRAFT_SUPABASE_URL`, `ANACRAFT_SUPABASE_KEY` and
`ANACRAFT_WEB_OAUTH_CLIENT_ID`. The Web OAuth client needs
`https://anacraft.dev` as an authorized JavaScript origin and
`https://anacraft.dev/app/` as a redirect URI.
