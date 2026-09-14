#!/usr/bin/env python3
"""Point the Payment Link's confirmation page at docs/success.html.

Stripe finishes a checkout on its own hosted confirmation page unless the link
says otherwise, and a link that redirects somewhere generic — the home page,
say — spends the one moment somebody is pleased with you showing them a page
written for strangers. `success.html` is the page for that moment: it says the
payment cleared, and it says the one thing the payment cannot do for itself,
which is `craft login`.

The redirect is a property of the Payment Link, not of a session, so it is the
same destination for a checkout started by `craft subscribe` and one started by
the Subscribe button on the pricing page. The page is written for both.

    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-success-url.py           # dry run
    STRIPE_SECRET_KEY=sk_live_... python3 scripts/stripe-success-url.py --apply

A restricted key with write on Payment Links is enough. Dry run by default: it
prints what the link does now and what it would do instead, and touches
nothing. Changing this affects the next real customer, not a test one.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from stripe_api import call, each  # noqa: E402

# The links `craft subscribe --plan <name>` opens and the pricing page's
# buttons point at. Matched on the metadata tag `stripe-plans.py` stamps on its
# own links rather than on `url`, because the short code in a URL is not the
# link's id and the Basic link predates tags and keeps working alongside.
PLANS = {"basic", "pro", "elite"}

# The Basic link predates the metadata tag, so it is recognised by its URL.
BASIC_LINK = "https://buy.stripe.com/3cIdR93sU4SbfECab79MY02"

# `{CHECKOUT_SESSION_ID}` is Stripe's own placeholder and it substitutes it on
# the way out. The page cannot ask Stripe anything about it — a static page has
# no secret key — but it costs nothing to carry, it is what turns a support
# email about a payment into a one-line lookup, and GA4 takes it as the
# transaction id, which is what stops one payment being counted as two.
#
# `plan` rides along because it is the only way the confirmation page can know
# what was bought. Stripe tells the page nothing else, and a `purchase` event
# with no amount on it is worse than none: it would report a sale worth zero
# and make every revenue figure on the property wrong in the direction that
# looks like the site is failing. The page fires nothing when this is missing.
SUCCESS_URL = "https://anacraft.dev/success.html?session_id={CHECKOUT_SESSION_ID}&plan=%s"


def plan_of(link):
    """Which plan a link sells, by its tag, or by name for the older one."""
    tagged = (link.get("metadata") or {}).get("plan")
    if tagged in PLANS:
        return tagged
    return "basic" if link.get("url") == BASIC_LINK else None

APPLY = "--apply" in sys.argv


def plan_links():
    found = []
    for link in each("payment_links"):
        if (link.get("metadata") or {}).get("plan") in PLANS:
            found.append(link)
    # The Basic link predates the metadata tag, so its URL names it here too,
    # exactly the way the first version of this script found it. Reading the
    # code from src/main.rs keeps it in one place and honest.
    bearer = None
    for link in each("payment_links"):
        if link.get("url") == BASIC_LINK:
            bearer = link
            break
    if bearer:
        found.append(bearer)
    return found


def describe(link):
    after = link.get("after_completion") or {}
    kind = after.get("type")
    if kind == "redirect":
        return f"redirects to {after.get('redirect', {}).get('url')}"
    if kind == "hosted_confirmation":
        return "shows Stripe's own confirmation page"
    return f"after_completion is {kind or 'unset'}"


def main():
    links = plan_links()
    if not links:
        sys.exit("no plan payment links to point at success.html")

    for link in links:
        plan = plan_of(link)
        if not plan:
            print(f"  link      {link['id']}  {link['url']}")
            print("  skipped   no plan on this link, and the page needs one")
            continue
        target = SUCCESS_URL % plan

        print(f"  link      {link['id']}  {link['url']}")
        print(f"  plan      {plan}")
        print(f"  now       {describe(link)}")
        print(f"  would be  redirects to {target}")

        if APPLY:
            updated = call("POST", f"payment_links/{link['id']}", {
                "after_completion[type]": "redirect",
                "after_completion[redirect][url]": target,
            })
            print(f"  done      {describe(updated)}")

    if not APPLY:
        print("\n  dry run — nothing changed. Re-run with --apply.")
        return
    print("\n  pay with a real <$5 card to check a link, then refund yourself in Stripe —")
    print("  a test-mode card cannot exercise a live link.")


if __name__ == "__main__":
    main()
