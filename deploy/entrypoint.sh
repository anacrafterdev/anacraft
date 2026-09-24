#!/bin/sh
# Start `craft serve` on loopback, then nginx in front of it.
set -eu

PORT="${CRAFT_PORT:-8787}"
PUBLIC_ORIGIN="${PUBLIC_ORIGIN:?PUBLIC_ORIGIN is required, e.g. https://app.anacraft.dev}"
: "${CRAFT_TOKEN:?CRAFT_TOKEN is required: the bearer token the page and API answer to}"

sed -e "s|__PUBLIC_ORIGIN__|${PUBLIC_ORIGIN}|g" -e "s|__PORT__|${PORT}|g" \
    /etc/nginx/craft.conf.in > /tmp/nginx.conf

set -- serve --port "$PORT" --no-open --idle 0 --token "$CRAFT_TOKEN"
if [ "${CRAFT_DEMO:-true}" = "true" ]; then
    set -- "$@" --demo
fi

craft "$@" &
craft_pid=$!

# Either one going away takes the container with it, so the kubelet restarts
# the pair rather than leaving nginx answering 502 in front of nothing.
nginx -c /tmp/nginx.conf -g 'daemon off;' &
nginx_pid=$!

trap 'kill -TERM "$craft_pid" "$nginx_pid" 2>/dev/null' TERM INT
while kill -0 "$craft_pid" 2>/dev/null && kill -0 "$nginx_pid" 2>/dev/null; do
    sleep 2
done
kill -TERM "$craft_pid" "$nginx_pid" 2>/dev/null || true
exit 1
