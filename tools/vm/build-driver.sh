#!/usr/bin/env bash
# Build a driver on sb-build from the current working tree and copy the package back.
#   tools/vm/build-driver.sh [--dir driver] [--out <package dir, repo-relative>] [-- build command...]
# Defaults: builds in ./driver with `cargo make`, fetches the newest target/**/*_package dir.
# Package lands in target/vm/<name>/ locally; pass that to deploy-driver.sh.
set -euo pipefail
source "$(dirname "$0")/config.sh"

DIR=driver; OUT=""; CMD="cargo make"
while (( $# )); do
  case $1 in
    --dir) DIR=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --) shift; CMD="$*"; break ;;
    *) die "unknown option $1" ;;
  esac
done

wait_ssh build 120

sync_tree

info "Building in $DIR: $CMD"
start=$SECONDS
vm_ps sb-build "Set-Location '$REMOTE_SRC\\${DIR//\//\\}'; $CMD; exit \$LASTEXITCODE" || die "build failed"
info "Build took $((SECONDS - start))s"

if [[ -z $OUT ]]; then
  OUT=$(vm_ps sb-build "
    Get-ChildItem '$REMOTE_SRC\\target' -Recurse -Directory -Filter '*_package' -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime | Select-Object -Last 1 |
      ForEach-Object { \$_.FullName.Substring('$REMOTE_SRC'.Length + 1) }")
  [[ -n $OUT ]] || die "no *_package directory found under target/; pass --out"
fi
OUT=${OUT//\\//}
LOCAL=$REPO_DIR/target/vm/$(basename "$OUT")
rm -rf "$LOCAL"; mkdir -p "$(dirname "$LOCAL")"
"${SCP[@]}" -q -r "sb-build:C:/src/soundboard/$OUT" "$LOCAL"
info "Package: ${LOCAL#$REPO_DIR/}"
ls -1 "$LOCAL"
