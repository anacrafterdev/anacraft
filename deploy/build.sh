#!/bin/bash
# Build the craft serve image and push it to GCR, tagged latest and with a
# date stamp — the same shape as ~/source/api/build.sh.
set -eo pipefail

if [[ -z $1 || -z $2 ]]; then
    printf '\n\tUsage: deploy/build.sh <gcp-project> <image-name> [registry] [version]\n\n'
    exit 1
fi

PROJECT_ID="$1"
IMAGE_NAME="$2"
REGISTRY=${3:-gcr.io}
VERSION=${4:-v$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../Cargo.toml" | head -1)}
DATE_STAMP=$(date +%Y%m%d-%H%M)
IMAGE="${REGISTRY}/${PROJECT_ID}/${IMAGE_NAME}"

gcloud auth configure-docker "${REGISTRY}" --quiet

printf '\nBuilding %s from release %s\n' "$IMAGE" "$VERSION"
docker buildx build --platform linux/amd64 \
    --build-arg VERSION="$VERSION" \
    -t "${IMAGE}:latest" -t "${IMAGE}:${DATE_STAMP}" -t "${IMAGE}:${VERSION}" \
    --push "$(dirname "$0")"

echo "Pushed ${IMAGE}:latest (${VERSION}, ${DATE_STAMP})"
