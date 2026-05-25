#!/usr/bin/env sh
set -eu

AGENTMESH_VERSION="${AGENTMESH_VERSION:-0.1.0}"
AGENTMESH_BASE_URL="${AGENTMESH_BASE_URL:-https://github.com/aranticlabs/agentmesh/releases/download}"
COSIGN_VERSION="${AGENTMESH_COSIGN_VERSION:-v2.6.3}"
COSIGN_CERTIFICATE_IDENTITY_REGEXP="${AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP:-^https://github.com/aranticlabs/agentmesh/.github/workflows/release.yml@refs/tags/(v|agentmesh-v).*}"
COSIGN_CERTIFICATE_OIDC_ISSUER="${AGENTMESH_COSIGN_CERTIFICATE_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"

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

detect_linux_abi() {
  if command -v ldd >/dev/null 2>&1; then
    ldd_version="$(ldd --version 2>&1 || true)"
    if printf '%s' "$ldd_version" | grep -qi musl; then
      printf 'unknown-linux-musl\n'
      return
    fi
    if [ -n "$ldd_version" ]; then
      printf 'unknown-linux-gnu\n'
      return
    fi
  fi
  if ls /lib/ld-musl-*.so.1 /usr/lib/ld-musl-*.so.1 >/dev/null 2>&1; then
    printf 'unknown-linux-musl\n'
    return
  fi
  printf 'unknown-linux-gnu\n'
}

detect_platform() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"

  case "$os" in
    darwin) os="apple-darwin" ;;
    linux) os="$(detect_linux_abi)" ;;
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

  if [ "$os" = "unknown-linux-musl" ] && [ "$arch" = "aarch64" ]; then
    echo "unsupported platform: aarch64-unknown-linux-musl" >&2
    exit 1
  fi

  printf '%s-%s\n' "$arch" "$os"
}

release_tag() {
  case "$channel" in
    stable) printf 'agentmesh-v%s\n' "$AGENTMESH_VERSION" ;;
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

bundle_url() {
  tag="$(release_tag)"
  printf '%s/%s/SHA256SUMS.bundle\n' "$AGENTMESH_BASE_URL" "$tag"
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

cosign_artifact_name() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$os" in
    darwin) os="darwin" ;;
    linux) os="linux" ;;
    msys*|mingw*|cygwin*) os="windows" ;;
    *) echo "unsupported operating system for cosign: $os" >&2; exit 1 ;;
  esac
  case "$arch" in
    x86_64|amd64) arch="amd64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) echo "unsupported architecture for cosign: $arch" >&2; exit 1 ;;
  esac
  suffix=""
  if [ "$os" = "windows" ]; then
    suffix=".exe"
  fi
  printf 'cosign-%s-%s%s\n' "$os" "$arch" "$suffix"
}

cosign_expected_sha256() {
  artifact="$1"
  if [ -n "${AGENTMESH_COSIGN_SHA256:-}" ]; then
    printf '%s\n' "$AGENTMESH_COSIGN_SHA256"
    return
  fi
  if [ "$COSIGN_VERSION" != "v2.6.3" ]; then
    echo "AGENTMESH_COSIGN_SHA256 is required when overriding AGENTMESH_COSIGN_VERSION" >&2
    exit 1
  fi
  case "$artifact" in
    cosign-darwin-amd64) printf '%s\n' "5715d61dd00a9b6dcb344de14910b434145855b7f82690b94183c553ac1b68be" ;;
    cosign-darwin-arm64) printf '%s\n' "ff497a698f125f3130b04f000b2cb0dd163bcaf00b5e776ef536035e6d0b3f3e" ;;
    cosign-linux-amd64) printf '%s\n' "7c78a7f2efc00088bd788a758db6e0928e79f3e0eb83eb5d3c499ed98da4c4f4" ;;
    cosign-linux-arm64) printf '%s\n' "b7c23659a50a59fd8eec44b87188e9062157d0c87796cac7b38727e5390c4917" ;;
    cosign-windows-amd64.exe) printf '%s\n' "2264ea5867077b9e070161648e8c18544decac351f5f3a7edaea43c233ce2e36" ;;
    *)
      echo "unsupported cosign artifact: $artifact" >&2
      exit 1
      ;;
  esac
}

verify_cosign_sha256() {
  file="$1"
  expected="$2"
  actual="$(sha256_file "$file")"
  if [ "$actual" != "$expected" ]; then
    echo "cosign sha256 mismatch for $file" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    exit 1
  fi
}

cosign_cache_dir() {
  if [ -n "${AGENTMESH_COSIGN_DIR:-}" ]; then
    printf '%s\n' "$AGENTMESH_COSIGN_DIR"
    return
  fi
  if [ -n "${XDG_CACHE_HOME:-}" ]; then
    printf '%s/agentmesh/cosign/%s\n' "$XDG_CACHE_HOME" "$COSIGN_VERSION"
    return
  fi
  if [ -n "${HOME:-}" ]; then
    printf '%s/.cache/agentmesh/cosign/%s\n' "$HOME" "$COSIGN_VERSION"
    return
  fi
  printf '%s/agentmesh-cosign/%s\n' "${TMPDIR:-/tmp}" "$COSIGN_VERSION"
}

prepare_cosign_cache_dir() {
  cosign_dir="$(cosign_cache_dir)"
  if [ -L "$cosign_dir" ]; then
    echo "cosign cache directory must not be a symlink: $cosign_dir" >&2
    exit 1
  fi
  mkdir -p "$cosign_dir"
  if [ ! -O "$cosign_dir" ]; then
    echo "cosign cache directory is not owned by the current user: $cosign_dir" >&2
    exit 1
  fi
  chmod 700 "$cosign_dir"
  printf '%s\n' "$cosign_dir"
}

cosign_command() {
  if [ -n "${AGENTMESH_COSIGN_BIN:-}" ]; then
    if [ ! -x "$AGENTMESH_COSIGN_BIN" ]; then
      echo "AGENTMESH_COSIGN_BIN is not executable: $AGENTMESH_COSIGN_BIN" >&2
      exit 1
    fi
    printf '%s\n' "$AGENTMESH_COSIGN_BIN"
    return
  fi
  if command -v cosign >/dev/null 2>&1; then
    command -v cosign
    return
  fi
  artifact="$(cosign_artifact_name)"
  expected="$(cosign_expected_sha256 "$artifact")"
  cosign_dir="$(prepare_cosign_cache_dir)"
  cosign_bin="$cosign_dir/$artifact"
  if [ -e "$cosign_bin" ]; then
    if [ -L "$cosign_bin" ] || [ ! -f "$cosign_bin" ] || [ ! -O "$cosign_bin" ]; then
      echo "cosign cache entry is not a regular user-owned file: $cosign_bin" >&2
      exit 1
    fi
    if [ "$(sha256_file "$cosign_bin")" = "$expected" ]; then
      chmod +x "$cosign_bin"
      printf '%s\n' "$cosign_bin"
      return
    fi
    rm -f "$cosign_bin"
  fi
  cosign_base="${AGENTMESH_COSIGN_BASE_URL:-https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION}"
  temp_bin="$cosign_bin.tmp.$$"
  rm -f "$temp_bin"
  fetch_url "$cosign_base/$artifact" "$temp_bin"
  verify_cosign_sha256 "$temp_bin" "$expected"
  chmod +x "$temp_bin"
  mv "$temp_bin" "$cosign_bin"
  printf '%s\n' "$cosign_bin"
}

verify_manifest_signature() {
  manifest="$1"
  signature="$2"
  bundle="$3"
  cosign="$(cosign_command)"
  "$cosign" verify-blob \
    --signature "$signature" \
    --bundle "$bundle" \
    --certificate-identity-regexp "$COSIGN_CERTIFICATE_IDENTITY_REGEXP" \
    --certificate-oidc-issuer "$COSIGN_CERTIFICATE_OIDC_ISSUER" \
    "$manifest" >/dev/null
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
  binary_name="agentmesh"
  case "$platform" in
    *windows*) binary_name="agentmesh.exe" ;;
  esac
  tag="$(release_tag)"
  workdir="$(make_workdir)"
  trap 'rm -rf "$workdir"' EXIT HUP INT TERM
  archive="$workdir/$artifact"
  manifest="$workdir/SHA256SUMS"
  signature="$workdir/SHA256SUMS.sig"
  bundle="$workdir/SHA256SUMS.bundle"

  fetch_url "$AGENTMESH_BASE_URL/$tag/SHA256SUMS" "$manifest"
  fetch_url "$AGENTMESH_BASE_URL/$tag/SHA256SUMS.sig" "$signature"
  fetch_url "$AGENTMESH_BASE_URL/$tag/SHA256SUMS.bundle" "$bundle"
  verify_manifest_signature "$manifest" "$signature" "$bundle"
  fetch_url "$AGENTMESH_BASE_URL/$tag/$artifact" "$archive"
  verify_sha256sums "$archive" "$manifest" "$artifact"

  safe_extract_archive "$archive" "$workdir/extract"
  binary="$workdir/extract/agentmesh/$binary_name"
  if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    binary="$(find "$workdir/extract" -type f -name "$binary_name" -print | head -n 1)"
  fi
  if [ -z "$binary" ] || [ ! -f "$binary" ]; then
    echo "$binary_name binary not found in $artifact" >&2
    exit 1
  fi

  mkdir -p "$install_dir"
  chmod +x "$binary"
  cp "$binary" "$install_dir/$binary_name"
  echo "Installed agentmesh to $install_dir/$binary_name"
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
verify_signature_file=""
verify_bundle_file=""
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
    --verify-sha256sums-signature)
      if [ "$#" -lt 4 ]; then
        echo "usage: install.sh --verify-sha256sums-signature <SHA256SUMS> <SHA256SUMS.sig> <SHA256SUMS.bundle>" >&2
        exit 64
      fi
      command="verify-sha256sums-signature"
      verify_manifest_file="$2"
      verify_signature_file="$3"
      verify_bundle_file="$4"
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
  install.sh --verify-sha256sums-signature <SHA256SUMS> <SHA256SUMS.sig> <SHA256SUMS.bundle>
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
    printf 'bundle_url=%s\n' "$(bundle_url)"
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
  verify-sha256sums-signature)
    verify_manifest_signature "$verify_manifest_file" "$verify_signature_file" "$verify_bundle_file"
    exit 0
    ;;
esac

install_archive
