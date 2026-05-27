#!/usr/bin/env sh
set -eu

version="${1:-}"
manifest="${2:-Cargo.toml}"
lockfile="${3:-Cargo.lock}"
release_manifest="${4:-.release-please-manifest.json}"

if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "usage: $0 X.Y.Z [Cargo.toml] [Cargo.lock] [.release-please-manifest.json]" >&2
  exit 64
fi

VERSION="$version" perl -0pi -e '
  my $version = $ENV{"VERSION"};
  s/(\[workspace\.package\]\n(?:[^\[]*\n)*?version = ")[^"]+(")/$1 . $version . $2/e;
  for my $crate (
    "agentmesh-adapter-claude",
    "agentmesh-adapter-codex",
    "agentmesh-adapter-sdk-rust",
    "agentmesh-core",
    "agentmesh-protocol",
    "agentmesh-watcher",
  ) {
    my $quoted = quotemeta($crate);
    s/($quoted = \{ path = "[^"]+", version = ")[^"]+(" \})/$1 . $version . $2/ge;
  }
' "$manifest"

VERSION="$version" perl -0pi -e '
  my $version = $ENV{"VERSION"};
  for my $crate (
    "agentmesh",
    "agentmesh-adapter-claude",
    "agentmesh-adapter-codex",
    "agentmesh-adapter-sdk-rust",
    "agentmesh-core",
    "agentmesh-protocol",
    "agentmesh-watcher",
  ) {
    my $quoted = quotemeta($crate);
    s/(\[\[package\]\]\nname = "$quoted"\nversion = ")[^"]+(")/$1 . $version . $2/ge;
  }
' "$lockfile"

if [ -f "$release_manifest" ]; then
  VERSION="$version" perl -0pi -e '
    my $version = $ENV{"VERSION"};
    s/("\."\s*:\s*")[^"]+(")/$1 . $version . $2/e;
  ' "$release_manifest"
fi

VERSION="$version" perl -0ne '
  my $version = $ENV{"VERSION"};
  my @missing;
  push @missing, "workspace.package version" unless /\[workspace\.package\]\n(?:[^\[]*\n)*?version = "\Q$version\E"/;
  for my $crate (
    "agentmesh-adapter-claude",
    "agentmesh-adapter-codex",
    "agentmesh-adapter-sdk-rust",
    "agentmesh-core",
    "agentmesh-protocol",
    "agentmesh-watcher",
  ) {
    push @missing, "dependency $crate" unless /\Q$crate\E = \{ path = "[^"]+", version = "\Q$version\E" \}/;
  }
  if (@missing) {
    print STDERR "failed to update Cargo.toml: " . join(", ", @missing) . "\n";
    exit 65;
  }
' "$manifest"

VERSION="$version" perl -0ne '
  my $version = $ENV{"VERSION"};
  my @missing;
  for my $crate (
    "agentmesh",
    "agentmesh-adapter-claude",
    "agentmesh-adapter-codex",
    "agentmesh-adapter-sdk-rust",
    "agentmesh-core",
    "agentmesh-protocol",
    "agentmesh-watcher",
  ) {
    push @missing, $crate unless /\[\[package\]\]\nname = "\Q$crate\E"\nversion = "\Q$version\E"/;
  }
  if (@missing) {
    print STDERR "failed to update Cargo.lock: " . join(", ", @missing) . "\n";
    exit 65;
  }
' "$lockfile"

if [ -f "$release_manifest" ]; then
  VERSION="$version" perl -0ne '
    my $version = $ENV{"VERSION"};
    if (!/"\."\s*:\s*"\Q$version\E"/) {
      print STDERR "failed to update .release-please-manifest.json\n";
      exit 65;
    }
  ' "$release_manifest"
fi
