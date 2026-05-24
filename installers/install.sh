#!/usr/bin/env sh
set -eu

AGENTMESH_VERSION="${AGENTMESH_VERSION:-0.1.0}"
AGENTMESH_BASE_URL="${AGENTMESH_BASE_URL:-https://github.com/aranticlabs/agentmesh/releases/download}"

sha256_file() {
  file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return
  fi
  echo "no SHA-256 tool found; install sha256sum or shasum" >&2
  exit 1
}

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

detect_platform() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"

  case "$os" in
    darwin) os="apple-darwin" ;;
    linux) os="unknown-linux-gnu" ;;
    msys*|mingw*|cygwin*) os="pc-windows-msvc" ;;
    *)
      echo "unsupported operating system: $os" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *)
      echo "unsupported architecture: $arch" >&2
      exit 1
      ;;
  esac

  printf '%s-%s\n' "$arch" "$os"
}

release_tag() {
  case "$channel" in
    stable) printf 'agentmesh-cli-v%s\n' "$AGENTMESH_VERSION" ;;
    nightly) printf 'nightly\n' ;;
    *)
      echo "unsupported channel: $channel" >&2
      exit 64
      ;;
  esac
}

artifact_name() {
  platform="$1"
  case "$channel" in
    stable) printf 'agentmesh-v%s-%s.tar.gz\n' "$AGENTMESH_VERSION" "$platform" ;;
    nightly) printf 'agentmesh-nightly-%s.tar.gz\n' "$platform" ;;
  esac
}

artifact_url() {
  tag="$(release_tag)"
  artifact="$(artifact_name "$(detect_platform)")"
  printf '%s/%s/%s\n' "$AGENTMESH_BASE_URL" "$tag" "$artifact"
}

manifest_url() {
  tag="$(release_tag)"
  printf '%s/%s/SHA256SUMS\n' "$AGENTMESH_BASE_URL" "$tag"
}

signature_url() {
  tag="$(release_tag)"
  printf '%s/%s/SHA256SUMS.sig\n' "$AGENTMESH_BASE_URL" "$tag"
}

manifest_hash_for() {
  manifest="$1"
  artifact="$2"
  basename_artifact="$(basename "$artifact")"
  awk -v artifact="$artifact" -v basename_artifact="$basename_artifact" '
    $2 == artifact || $2 == basename_artifact || $2 == "./" basename_artifact || $2 == "*" basename_artifact {
      print $1
      exit
    }
  ' "$manifest"
}

verify_sha256() {
  file="$1"
  expected="$2"
  actual="$(sha256_file "$file")"
  if [ "$actual" != "$expected" ]; then
    echo "sha256 mismatch for $file" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    exit 1
  fi
  echo "sha256 verified: $file"
}

verify_sha256sums() {
  file="$1"
  manifest="$2"
  artifact="$3"
  expected="$(manifest_hash_for "$manifest" "$artifact")"
  if [ -z "$expected" ]; then
    echo "artifact $artifact not found in $manifest" >&2
    exit 1
  fi
  verify_sha256 "$file" "$expected"
}

verify_manifest_signature() {
  manifest="$1"
  signature="$2"
  if ! command -v cosign >/dev/null 2>&1; then
    echo "cosign is required to verify SHA256SUMS signatures" >&2
    echo "install cosign, then retry" >&2
    exit 1
  fi
  cosign verify-blob --signature "$signature" "$manifest" >/dev/null
}

install_dir_default() {
  if [ -n "${HOME:-}" ]; then
    printf '%s\n' "$HOME/.local/bin"
    return
  fi
  if [ -w /usr/local/bin ]; then
    printf '%s\n' "/usr/local/bin"
    return
  fi
  echo "cannot determine install directory; pass --install-dir=<path>" >&2
  exit 1
}

make_workdir() {
  base="${TMPDIR:-/tmp}"
  workdir="$(mktemp -d "${base%/}/agentmesh-install.XXXXXX")"
  chmod 700 "$workdir"
  printf '%s\n' "$workdir"
}

safe_extract_archive() {
  archive="$1"
  destination="$2"
  if ! tar -tzf "$archive" | awk '
    $0 == "" || $0 ~ /^\// || $0 ~ /(^|\/)\.\.(\/|$)/ {
      print "unsafe archive member: " $0 > "/dev/stderr"
      unsafe = 1
    }
    END { exit unsafe ? 1 : 0 }
  '; then
    echo "refusing to extract unsafe archive: $archive" >&2
    exit 1
  fi
  mkdir -p "$destination"
  tar -xzf "$archive" -C "$destination"
}

install_archive() {
  platform="$(detect_platform)"
  artifact="$(artifact_name "$platform")"
  tag="$(release_tag)"
  workdir="$(make_workdir)"
  trap 'rm -rf "$workdir"' EXIT HUP INT TERM
  archive="$workdir/$artifact"
  manifest="$workdir/SHA256SUMS"
  signature="$workdir/SHA256SUMS.sig"

  fetch_url "$AGENTMESH_BASE_URL/$tag/SHA256SUMS" "$manifest"
  fetch_url "$AGENTMESH_BASE_URL/$tag/SHA256SUMS.sig" "$signature"
  verify_manifest_signature "$manifest" "$signature"
  fetch_url "$AGENTMESH_BASE_URL/$tag/$artifact" "$archive"
  verify_sha256sums "$archive" "$manifest" "$artifact"

  safe_extract_archive "$archive" "$workdir/extract"
  binary="$workdir/extract/agentmesh"
  if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    binary="$(find "$workdir/extract" -type f -name agentmesh -print | head -n 1)"
  fi
  if [ -z "$binary" ] || [ ! -f "$binary" ]; then
    echo "agentmesh binary not found in $artifact" >&2
    exit 1
  fi

  mkdir -p "$install_dir"
  chmod +x "$binary"
  cp "$binary" "$install_dir/agentmesh"
  echo "Installed agentmesh to $install_dir/agentmesh"
  case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
      echo "Add $install_dir to PATH before running agentmesh from another shell." >&2
      ;;
  esac
}

channel="stable"
command="install"
verify_file=""
verify_expected=""
verify_manifest_file=""
verify_artifact=""
install_dir="$(install_dir_default)"

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help)
      command="help"
      shift
      ;;
    --smoke)
      command="smoke"
      shift
      ;;
    --upgrade-help)
      command="upgrade-help"
      shift
      ;;
    --print-platform)
      command="print-platform"
      shift
      ;;
    --print-url)
      command="print-url"
      shift
      ;;
    --verify-sha256)
      if [ "$#" -lt 3 ]; then
        echo "usage: install.sh --verify-sha256 <file> <expected-sha256>" >&2
        exit 64
      fi
      command="verify-sha256"
      verify_file="$2"
      verify_expected="$3"
      shift 3
      ;;
    --verify-sha256sums)
      if [ "$#" -lt 4 ]; then
        echo "usage: install.sh --verify-sha256sums <file> <SHA256SUMS> <artifact-name>" >&2
        exit 64
      fi
      command="verify-sha256sums"
      verify_file="$2"
      verify_manifest_file="$3"
      verify_artifact="$4"
      shift 4
      ;;
    --channel=stable)
      channel="stable"
      shift
      ;;
    --channel=nightly)
      channel="nightly"
      shift
      ;;
    --install-dir=*)
      install_dir="${1#--install-dir=}"
      shift
      ;;
    *)
      echo "unsupported option: $1" >&2
      exit 64
      ;;
  esac
done

case "$command" in
  help)
    cat <<'USAGE'
AgentMesh installer

Usage:
  install.sh [--channel=stable|--channel=nightly] [--install-dir=<path>]
  install.sh --print-platform
  install.sh --print-url [--channel=stable|--channel=nightly]
  install.sh --verify-sha256 <file> <expected-sha256>
  install.sh --verify-sha256sums <file> <SHA256SUMS> <artifact-name>
  install.sh --upgrade-help
  install.sh --smoke

The installer downloads the platform archive, verifies it against SHA256SUMS,
verifies the SHA256SUMS signature with cosign, and installs the single binary.
USAGE
    exit 0
    ;;
  smoke)
    platform="$(detect_platform)"
    printf 'agentmesh installer smoke ok (channel=%s platform=%s artifact=%s)\n' "$channel" "$platform" "$(artifact_name "$platform")"
    exit 0
    ;;
  upgrade-help)
    cat <<'USAGE'
AgentMesh upgrade flow

1. Upgrade the binary with your package manager or this installer.
2. Run `agentmesh upgrade` in each repository that has hooks installed.
3. Review the printed path and hash summary before continuing hook-driven sync.
USAGE
    exit 0
    ;;
  print-platform)
    detect_platform
    exit 0
    ;;
  print-url)
    printf 'artifact_url=%s\n' "$(artifact_url)"
    printf 'sha256sums_url=%s\n' "$(manifest_url)"
    printf 'signature_url=%s\n' "$(signature_url)"
    exit 0
    ;;
  verify-sha256)
    verify_sha256 "$verify_file" "$verify_expected"
    exit 0
    ;;
  verify-sha256sums)
    verify_sha256sums "$verify_file" "$verify_manifest_file" "$verify_artifact"
    exit 0
    ;;
esac

install_archive
