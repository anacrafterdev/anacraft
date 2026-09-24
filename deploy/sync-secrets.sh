#!/bin/bash
# Write helm/app/values.secrets.<env>.yaml from Secret Manager, the way
# ~/source/api/scripts/generate_helm_secrets.sh does.
#
# The two secrets are the Google *Web application* OAuth client hosted
# visitors sign in with — a client of its own, beside the Desktop one the CLI
# has compiled in, because Google only sends a Desktop client back to
# loopback. They are made by hand in the Cloud Console, so a missing one is
# an error with instructions rather than something to mint.
set -euo pipefail
umask 077

ENV=${ENV:-prod}
PROJECT_ID=${PROJECT_ID:?Missing PROJECT_ID}
OUT_FILE="helm/app/values.secrets.${ENV}.yaml"
PREFIX="${PROJECT_ID}-${ENV}-managed-anacraft"

fetch() {
    gcloud secrets versions access latest --secret="${PREFIX}-$1" --project="$PROJECT_ID" 2>/dev/null
}

if ! ID=$(fetch web-oauth-client-id) || ! SECRET=$(fetch web-oauth-client-secret); then
    # A demo deploy signs nobody in, so it needs none of this.
    if [ "${DEMO:-false}" = "true" ]; then
        printf 'env: {}\n' > "$OUT_FILE"
        echo "  ➖ no Web OAuth client — fine for a demo deploy"
        exit 0
    fi
    cat >&2 <<MSG
  ❌ ${PREFIX}-web-oauth-client-id / -secret not found.

  Create a Web application OAuth client in the Cloud Console (redirect URI
  https://app.anacraft.dev/oauth/callback), then:

    printf '%s' '<client id>'     | gcloud secrets create ${PREFIX}-web-oauth-client-id     --project=${PROJECT_ID} --data-file=-
    printf '%s' '<client secret>' | gcloud secrets create ${PREFIX}-web-oauth-client-secret --project=${PROJECT_ID} --data-file=-
MSG
    exit 1
fi

# The MCP connector. The grant key seals users' Google refresh tokens before
# they go to Supabase; it is minted here the first time and kept in Secret
# Manager, never in the database. The service key is Supabase's own, copied
# by hand from the project's API settings. Without it the connector is off
# and the pages still work.
if ! GRANT_KEY=$(fetch grant-key); then
    echo "  ➕ ${PREFIX}-grant-key not found — minting it"
    GRANT_KEY=$(openssl rand -base64 32)
    gcloud secrets create "${PREFIX}-grant-key" --project="$PROJECT_ID" --replication-policy=automatic --quiet
    printf '%s' "$GRANT_KEY" | gcloud secrets versions add "${PREFIX}-grant-key" --project="$PROJECT_ID" --data-file=- --quiet
fi
SERVICE_KEY=$(fetch supabase-service-key || true)

{
    printf 'env:\n'
    printf '  ANACRAFT_WEB_OAUTH_CLIENT_ID: "%s"\n' "$ID"
    printf '  ANACRAFT_WEB_OAUTH_CLIENT_SECRET: "%s"\n' "$SECRET"
    if [ -n "$SERVICE_KEY" ]; then
        printf '  ANACRAFT_SUPABASE_SERVICE_KEY: "%s"\n' "$SERVICE_KEY"
        printf '  ANACRAFT_GRANT_KEY: "%s"\n' "$GRANT_KEY"
    fi
} > "$OUT_FILE"
echo "  ✅ Web OAuth client → $OUT_FILE"
if [ -n "$SERVICE_KEY" ]; then
    echo "  ✅ MCP connector on (grant key + Supabase service key)"
else
    echo "  ➖ ${PREFIX}-supabase-service-key not found — the MCP connector stays off."
    echo "     printf '%s' '<service_role key>' | gcloud secrets create ${PREFIX}-supabase-service-key --project=${PROJECT_ID} --data-file=-"
fi
