#!/bin/bash
# Reserve the global static IP the Ingress names, and point the host at it
# in Cloudflare. DNS only (grey cloud): Google's managed certificate is
# validated against the load balancer directly, and Cloudflare's proxy in the
# way stops it from ever provisioning.
set -euo pipefail

PROJECT_ID=${PROJECT_ID:?Missing PROJECT_ID}
IP_NAME=${IP_NAME:?Missing IP_NAME}
HOST=${HOST:?Missing HOST}
ZONE=${ZONE:-anacraft.dev}

if ! gcloud compute addresses describe "$IP_NAME" --global --project="$PROJECT_ID" >/dev/null 2>&1; then
    echo "  ➕ reserving global static IP $IP_NAME"
    gcloud compute addresses create "$IP_NAME" --global --project="$PROJECT_ID" --quiet
fi
IP=$(gcloud compute addresses describe "$IP_NAME" --global --project="$PROJECT_ID" --format='value(address)')
echo "  🌐 $IP_NAME = $IP"

# Without a token the record is left to whoever holds the dashboard; say
# exactly what it has to be rather than refusing to deploy.
if [ -z "${CLOUDFLARE_API_TOKEN:-}" ]; then
    echo "  ➖ CLOUDFLARE_API_TOKEN not set — add this in Cloudflare yourself:"
    echo "     A  $HOST  →  $IP  (DNS only, grey cloud)"
    exit 0
fi

CF=https://api.cloudflare.com/client/v4
auth=(-H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H "Content-Type: application/json")

ZONE_ID=$(curl -fsS "${auth[@]}" "$CF/zones?name=$ZONE" | jq -r '.result[0].id // empty')
[ -n "$ZONE_ID" ] || { echo "  ❌ zone $ZONE not visible to this token"; exit 1; }

RECORD_ID=$(curl -fsS "${auth[@]}" "$CF/zones/$ZONE_ID/dns_records?type=A&name=$HOST" | jq -r '.result[0].id // empty')
BODY=$(jq -nc --arg name "$HOST" --arg ip "$IP" '{type:"A",name:$name,content:$ip,ttl:300,proxied:false}')

if [ -n "$RECORD_ID" ]; then
    curl -fsS "${auth[@]}" -X PUT "$CF/zones/$ZONE_ID/dns_records/$RECORD_ID" --data "$BODY" | jq -e '.success' >/dev/null
    echo "  ✅ updated A $HOST → $IP (DNS only)"
else
    curl -fsS "${auth[@]}" -X POST "$CF/zones/$ZONE_ID/dns_records" --data "$BODY" | jq -e '.success' >/dev/null
    echo "  ✅ created A $HOST → $IP (DNS only)"
fi
