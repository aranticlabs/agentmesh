#!/usr/bin/env sh
set -eu

script_dir="$(CDPATH='' cd "$(dirname "$0")" && pwd)"
tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/agentmesh-installer-test.XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

functions_file="$tmpdir/install-functions.sh"
awk '/^channel=/ { exit } { print }' "$script_dir/install.sh" > "$functions_file"

write_fake_uname() {
  directory="$1"
  cat > "$directory/uname" <<'EOF'
#!/usr/bin/env sh
if [ "${1:-}" = "-s" ]; then
  printf '%s\n' Darwin
  exit 0
fi
exit 1
EOF
  chmod +x "$directory/uname"
}

write_fake_codesign() {
  directory="$1"
  cat > "$directory/codesign" <<'EOF'
#!/usr/bin/env sh
for last do :; done
: > "$last.signed"
EOF
  chmod +x "$directory/codesign"
}

test_macos_sigkill_repairs_and_retries() {
  case_dir="$tmpdir/sigkill"
  mkdir -p "$case_dir"
  write_fake_uname "$case_dir"
  write_fake_codesign "$case_dir"
  cat > "$case_dir/agentmesh" <<'EOF'
#!/usr/bin/env sh
if [ -f "$0.signed" ]; then
  printf '%s\n' "agentmesh test"
  exit 0
fi
exit 137
EOF
  chmod +x "$case_dir/agentmesh"

  PATH="$case_dir:$PATH" sh -c \
    '. "$1"; verify_installed_binary_launches "$2"; test -f "$2.signed"' \
    sh "$functions_file" "$case_dir/agentmesh"
}

test_non_sigkill_status_is_preserved() {
  case_dir="$tmpdir/non-sigkill"
  mkdir -p "$case_dir"
  write_fake_uname "$case_dir"
  write_fake_codesign "$case_dir"
  cat > "$case_dir/agentmesh" <<'EOF'
#!/usr/bin/env sh
exit 42
EOF
  chmod +x "$case_dir/agentmesh"

  PATH="$case_dir:$PATH" sh -c '
    . "$1"
    set +e
    verify_installed_binary_launches "$2"
    status="$?"
    set -e
    [ "$status" -eq 42 ]
    [ ! -f "$2.signed" ]
  ' sh "$functions_file" "$case_dir/agentmesh"
}

test_success_does_not_codesign() {
  case_dir="$tmpdir/success"
  mkdir -p "$case_dir"
  write_fake_uname "$case_dir"
  write_fake_codesign "$case_dir"
  cat > "$case_dir/agentmesh" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "agentmesh test"
EOF
  chmod +x "$case_dir/agentmesh"

  PATH="$case_dir:$PATH" sh -c \
    '. "$1"; verify_installed_binary_launches "$2"; [ ! -f "$2.signed" ]' \
    sh "$functions_file" "$case_dir/agentmesh"
}

test_macos_sigkill_repairs_and_retries
test_non_sigkill_status_is_preserved
test_success_does_not_codesign

printf '%s\n' "agentmesh installer tests ok"
