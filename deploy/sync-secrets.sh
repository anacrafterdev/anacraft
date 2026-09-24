#!/bin/bash
# Write helm/app/values.secrets.<env>.yaml from Secret Manager, the way
# ~/source/api/scripts/generate_helm_secrets.sh does. The one secret is the
# bearer token craft serve answers to; it is minted into Secret Manager the
# first time, so the link stays the same across deploys.
set -euo pipefail

ENV=${ENV:-prod}
PROJECT_ID=${PROJECT_ID:?Missing PROJECT_ID}
OUT_FILE="helm/app/values.secrets.${ENV}.yaml"
SECRET="${PROJECT_ID}-${ENV}-managed-anacraft-craft-token"

if ! TOKEN=$(gcloud secrets versions access latest --secret="$SECRET" --project="$PROJECT_ID" 2>/dev/null); then
    echo "  ➕ $SECRET not found — minting it"
    TOKEN=$(openssl rand -hex 20)
    gcloud secrets create "$SECRET" --project="$PROJECT_ID" --replication-policy=automatic --quiet
    printf '%s' "$TOKEN" | gcloud secrets versions add "$SECRET" --project="$PROJECT_ID" --data-file=- --quiet
fi

printf 'env:\n  CRAFT_TOKEN: "%s"\n' "$TOKEN" > "$OUT_FILE"
echo "  ✅ CRAFT_TOKEN → $OUT_FILE"
