#!/bin/sh
# craft serve as a hosted server: listening for the load balancer, answering
# the public origin, each visitor on a session of their own.
set -eu

: "${PUBLIC_ORIGIN:?PUBLIC_ORIGIN is required, e.g. https://app.anacraft.dev}"

set -- serve --host 0.0.0.0 --port 8080 --public-url "$PUBLIC_ORIGIN" --no-open
if [ "${CRAFT_DEMO:-false}" = "true" ]; then
    set -- "$@" --demo
fi
exec craft "$@"
