#!/usr/bin/env bash
# Run cargo on sb-build against the current working tree (user-mode crates: engine, controller).
#   tools/vm/win-cargo.sh test -p sb-engine
#   tools/vm/win-cargo.sh run -p sb-engine --release --example passthrough -- --list
# Output streams back live; exits with cargo's exit code.
set -euo pipefail
source "$(dirname "$0")/config.sh"
(( $# )) || die "usage: win-cargo.sh <cargo args...>"

wait_ssh build 120
sync_tree
# Quote each argument for PowerShell ('...' with '' escaping) so spaces and $ survive.
args=""
for a in "$@"; do args+=" '${a//\'/\'\'}'"; done
info "cargo$args"
vm_ps sb-build "Set-Location '$REMOTE_SRC'; cargo$args; exit \$LASTEXITCODE" || exit $?   # cargo's own error is enough
