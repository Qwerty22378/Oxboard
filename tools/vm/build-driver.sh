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
REMOTE='C:\src\soundboard'

wait_ssh build 120

info "Syncing working tree to sb-build"
tgz=$(mktemp --suffix=.tgz); trap 'rm -f "$tgz"' EXIT
git -C "$REPO_DIR" ls-files -z --cached --others --exclude-standard | tar -C "$REPO_DIR" --null -T - -czf "$tgz"
"${SCP[@]}" -q "$tgz" sb-build:C:/src/soundboard.tgz
# Replace everything except the build cache (target/), so deleted files don't linger.
vm_ps sb-build "
  New-Item -ItemType Directory -Force '$REMOTE' | Out-Null
  Get-ChildItem '$REMOTE' -Force | Where-Object Name -ne 'target' | Remove-Item -Recurse -Force
  tar -xzf C:\src\soundboard.tgz -C '$REMOTE'
  if (\$LASTEXITCODE) { exit 1 }"

info "Building in $DIR: $CMD"
start=$SECONDS
vm_ps sb-build "Set-Location '$REMOTE\\${DIR//\//\\}'; $CMD; exit \$LASTEXITCODE" || die "build failed"
info "Build took $((SECONDS - start))s"

if [[ -z $OUT ]]; then
  OUT=$(vm_ps sb-build "
    Get-ChildItem '$REMOTE\\target' -Recurse -Directory -Filter '*_package' -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime | Select-Object -Last 1 |
      ForEach-Object { \$_.FullName.Substring('$REMOTE'.Length + 1) }")
  [[ -n $OUT ]] || die "no *_package directory found under target/; pass --out"
fi
OUT=${OUT//\\//}
LOCAL=$REPO_DIR/target/vm/$(basename "$OUT")
rm -rf "$LOCAL"; mkdir -p "$(dirname "$LOCAL")"
"${SCP[@]}" -q -r "sb-build:C:/src/soundboard/$OUT" "$LOCAL"
info "Package: ${LOCAL#$REPO_DIR/}"
ls -1 "$LOCAL"
