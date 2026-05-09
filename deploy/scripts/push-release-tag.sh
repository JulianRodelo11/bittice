#!/usr/bin/env bash
# Push a Git tag that triggers `.github/workflows/release.yml` → GHCR multi-arch image
# ghcr.io/<owner>/bittice:<tag>
#
# Usage (repo root):
#   ./deploy/scripts/push-release-tag.sh              # tag v + version from Cargo.toml
#   ./deploy/scripts/push-release-tag.sh v0.1.67      # explicit tag (must match v* or beta-v*)
#
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "error: run from inside the bittice git repository" >&2
  exit 1
}
cd "$ROOT"

CARGO_TOML="$ROOT/Cargo.toml"
VERSION="$(grep -E '^version[[:space:]]*=' "$CARGO_TOML" | head -1 | sed -E 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/')"

TAG="${1:-v${VERSION}}"

case "$TAG" in
  v*|beta-v*) ;;
  *)
    echo "error: tag must start with 'v' or 'beta-v' (Actions only builds on those prefixes); got: $TAG" >&2
    exit 1
    ;;
esac

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "error: tag '$TAG' already exists locally" >&2
  exit 1
fi

if git ls-remote origin "refs/tags/${TAG}" 2>/dev/null | grep -q .; then
  echo "error: tag '$TAG' already exists on remote" >&2
  exit 1
fi

EXPECTED="v${VERSION}"
if [[ -z "${1:-}" && "$TAG" != "$EXPECTED" ]]; then
  echo "error: derived tag '$TAG' from Cargo.toml; unexpected parse" >&2
  exit 1
fi

if [[ -n "${1:-}" && "$TAG" != "$EXPECTED" ]]; then
  echo "warning: Cargo.toml version is ${VERSION}; you are tagging ${TAG}. Bump Cargo.toml first unless this is intentional (e.g. hotfix)." >&2
fi

echo "Creating annotated tag ${TAG} (triggers GHCR image build)"
git tag -a "$TAG" -m "Release ${TAG}"
echo "Pushing ${TAG} to origin…"
git push origin "$TAG"
echo ""
echo "Wait for GitHub Actions (Release workflow). Pull: ghcr.io/<owner>/<repo>:${TAG} — exact path under the repo Packages tab."
