#!/usr/bin/env sh
set -eu

AGENTMESH_CHANNEL="dev"
AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP="${AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP:-^https://github.com/aranticlabs/agentmesh/.github/workflows/dev-release.yml@refs/heads/dev$}"
export AGENTMESH_CHANNEL AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP

installer_url="${AGENTMESH_DEV_INSTALLER_URL:-https://raw.githubusercontent.com/aranticlabs/agentmesh/dev/installers/install.sh}"

fetch_url() {
  url="$1"
  output="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$output"
    return
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$output"
    return
  fi
  echo "no download tool found; install curl or wget" >&2
  exit 1
}

case "$0" in
  */*)
    script_dir="$(CDPATH='' cd "$(dirname "$0")" && pwd)"
    if [ -f "$script_dir/install.sh" ]; then
      exec sh "$script_dir/install.sh" "$@"
    fi
    ;;
esac

workdir="$(mktemp -d "${TMPDIR:-/tmp}/agentmesh-install-dev.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT HUP INT TERM
installer="$workdir/install.sh"
fetch_url "$installer_url" "$installer"
exec sh "$installer" "$@"
