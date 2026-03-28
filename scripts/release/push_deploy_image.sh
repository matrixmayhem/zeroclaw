#!/usr/bin/env bash
# Build and publish the Dockploy GHCR image from a laptop.
# Requires Docker Buildx and a classic PAT with write:packages.
# Add read:packages too if you want to verify pulls with the same token.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"

REGISTRY="${REGISTRY:-ghcr.io}"
IMAGE_NAME="${IMAGE_NAME:-matrixmayhem/zeroclaw}"
EDGE_TAG="${EDGE_TAG:-edge-debian}"
REVISION="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
SHORT_SHA="$(git -C "${REPO_ROOT}" rev-parse --short=12 HEAD)"
SHA_TAG="${SHA_TAG:-sha-${SHORT_SHA}-debian}"
PLATFORMS="${PLATFORMS:-linux/amd64,linux/arm64}"
PUSH="${PUSH:-1}"
OCI_SOURCE="${OCI_SOURCE:-https://github.com/matrixmayhem/zeroclaw}"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required" >&2
  exit 1
fi

if ! docker buildx version >/dev/null 2>&1; then
  echo "docker buildx is required" >&2
  exit 1
fi

if [[ "${PUSH}" == "1" ]]; then
  : "${GHCR_USERNAME:?Set GHCR_USERNAME to your GitHub username}"
  : "${GHCR_TOKEN:?Set GHCR_TOKEN to a classic PAT with write:packages}"
  printf '%s' "${GHCR_TOKEN}" | docker login "${REGISTRY}" -u "${GHCR_USERNAME}" --password-stdin
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT

docker_ctx="${tmpdir}/docker-ctx"
mkdir -p "${docker_ctx}/bin"

IFS=',' read -r -a platforms <<< "${PLATFORMS}"
for platform in "${platforms[@]}"; do
  arch="${platform##*/}"
  export_dir="${tmpdir}/export/${arch}"
  mkdir -p "${export_dir}" "${docker_ctx}/bin/${arch}"

  echo "Exporting builder output for ${platform}"
  docker buildx build \
    --file "${REPO_ROOT}/Dockerfile" \
    --target builder \
    --platform "${platform}" \
    --output "type=local,dest=${export_dir}" \
    "${REPO_ROOT}" >/dev/null

  if [[ ! -f "${export_dir}/app/zeroclaw" ]]; then
    echo "missing zeroclaw binary for ${platform}" >&2
    exit 1
  fi

  cp "${export_dir}/app/zeroclaw" "${docker_ctx}/bin/${arch}/zeroclaw"
  if [[ ! -d "${docker_ctx}/zeroclaw-data" ]]; then
    cp -R "${export_dir}/zeroclaw-data" "${docker_ctx}/zeroclaw-data"
  fi
done

cp "${REPO_ROOT}/Dockerfile.debian.ci" "${docker_ctx}/Dockerfile.debian"

echo "Prepared Docker context at ${docker_ctx}"
echo "Image tags:"
echo "  ${REGISTRY}/${IMAGE_NAME}:${EDGE_TAG}"
echo "  ${REGISTRY}/${IMAGE_NAME}:${SHA_TAG}"

if [[ "${PUSH}" != "1" ]]; then
  echo "PUSH=0, skipping final image build/push after context assembly."
  exit 0
fi

docker buildx build \
  --file "${docker_ctx}/Dockerfile.debian" \
  --platform "${PLATFORMS}" \
  --build-arg "OCI_SOURCE=${OCI_SOURCE}" \
  --build-arg "OCI_REVISION=${REVISION}" \
  --label "org.opencontainers.image.source=${OCI_SOURCE}" \
  --label "org.opencontainers.image.revision=${REVISION}" \
  --label "org.opencontainers.image.title=zeroclaw" \
  --tag "${REGISTRY}/${IMAGE_NAME}:${EDGE_TAG}" \
  --tag "${REGISTRY}/${IMAGE_NAME}:${SHA_TAG}" \
  --push \
  "${docker_ctx}"

echo "Pushed ${REGISTRY}/${IMAGE_NAME}:${EDGE_TAG}"
echo "Pushed ${REGISTRY}/${IMAGE_NAME}:${SHA_TAG}"
