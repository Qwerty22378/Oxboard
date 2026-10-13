#!/usr/bin/env bash
# End-to-end pipeline check (milestone D1): build Microsoft's C ACX audio sample on sb-build,
# deploy it to a clean sb-target, check the device comes up. Proves toolchain, test signing,
# packaging, deploy and revert all work before any Rust driver exists.
#   tools/vm/smoke-test.sh [--verifier]
set -euo pipefail
source "$(dirname "$0")/config.sh"
EXTRA=("$@")

wait_ssh build 120
info "Building the ACX sample on sb-build"
out=$(vm_ps sb-build '
  $ErrorActionPreference = "Stop"
  $src = "C:\src\Windows-driver-samples"
  # git reports progress on stderr, which Windows PowerShell turns into a terminating error under
  # "Stop". Run it through cmd and check exit codes instead. Re-clone if a previous clone is incomplete.
  if (-not (Test-Path "$src\audio\Acx")) {
    Remove-Item -Recurse -Force $src -ErrorAction SilentlyContinue
    cmd /c "git clone --quiet --depth 1 --filter=blob:none --sparse https://github.com/microsoft/Windows-driver-samples $src 2>&1"
    if ($LASTEXITCODE) { throw "git clone failed" }
    cmd /c "git -C $src sparse-checkout set audio/Acx 2>&1"
    if ($LASTEXITCODE) { throw "git sparse-checkout failed" }
  }
  $sln = Get-ChildItem "$src\audio\Acx\Samples" -Recurse -Filter *.sln | Select-Object -First 1
  if (-not $sln) { throw "no ACX sample solution found" }
  $msbuild = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -find "MSBuild\**\Bin\amd64\MSBuild.exe" | Select-Object -First 1
  & $msbuild $sln.FullName /m /nologo /v:minimal /p:Configuration=Debug /p:Platform=x64 | Out-Host
  if ($LASTEXITCODE) { throw "msbuild failed" }
  # The package dir is the one holding .inf + .sys + .cat together.
  $pkg = Get-ChildItem $sln.DirectoryName -Recurse -Filter *.cat | ForEach-Object Directory |
    Where-Object { (Get-ChildItem $_ -Filter *.inf) -and (Get-ChildItem $_ -Filter *.sys) } | Select-Object -First 1
  if (-not $pkg) { throw "no package dir (.inf+.sys+.cat) found" }
  "PKG=" + $pkg.FullName')
echo "$out" | grep -v '^PKG='
pkg=$(echo "$out" | sed -n 's/^PKG=//p'); [[ -n $pkg ]] || die "build didn't report a package"

LOCAL=$REPO_DIR/target/vm/acx-sample
rm -rf "$LOCAL"; mkdir -p "$(dirname "$LOCAL")"
"${SCP[@]}" -q -r "sb-build:${pkg//\\//}" "$LOCAL"
# WDK-stamped INFs are UTF-16LE (BOM FF FE); decode before grepping.
inf_text() { for f; do if [[ $(head -c2 "$f" | od -An -tx1) == " ff fe" ]]; then iconv -f UTF-16 -t UTF-8 "$f"; else cat "$f"; fi; done; }
hwid=$(inf_text "$LOCAL"/*.inf | grep -ohiE 'Root\\[A-Za-z0-9_]+' | head -1 || true)
[[ -n $hwid ]] || die "couldn't find a Root\\ hardware ID in the sample's .inf"
info "Package: ${LOCAL#$REPO_DIR/}  hardware ID: $hwid"

"$VM_DIR/deploy-driver.sh" "$LOCAL" --hwid "$hwid" "${EXTRA[@]}"

info "Audio endpoints on sb-target:"
vm_ps sb-target 'Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue | Format-Table -AutoSize Status, FriendlyName'
