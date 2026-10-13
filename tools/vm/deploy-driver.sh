#!/usr/bin/env bash
# Install a driver package on a clean sb-target.
#   tools/vm/deploy-driver.sh <package dir> [--hwid Root\Xyz] [--verifier] [--no-revert]
#   --hwid       create a root-enumerated device with this hardware ID (devgen), for software devices
#   --verifier   enable Driver Verifier (standard flags) on the package's .sys files, then reboot
#   --no-revert  install on top of the current state instead of reverting to the baseline first
# Attach the debugger in another terminal first (tools/vm/vm.sh kd) to catch bugchecks live.
set -euo pipefail
source "$(dirname "$0")/config.sh"

PKG=${1:-}; [[ -d $PKG ]] || die "usage: deploy-driver.sh <package dir> [--hwid ID] [--verifier] [--no-revert]"
shift
HWID=""; VERIFIER=0; REVERT=1
while (( $# )); do
  case $1 in
    --hwid) HWID=$2; shift 2 ;;
    --verifier) VERIFIER=1; shift ;;
    --no-revert) REVERT=0; shift ;;
    *) die "unknown option $1" ;;
  esac
done
ls "$PKG"/*.inf &>/dev/null || die "no .inf in $PKG"
NAME=$(basename "$PKG")
SYS=$(cd "$PKG" && ls *.sys 2>/dev/null | paste -sd' ' || true)

if (( REVERT )); then info "Reverting sb-target to its clean baseline"; "$VM_DIR/vm.sh" revert; else wait_ssh target 120; fi

info "Copying $NAME"
vm_ps sb-target "Remove-Item -Recurse -Force C:\\deploy\\$NAME -ErrorAction SilentlyContinue; New-Item -ItemType Directory -Force C:\\deploy | Out-Null"
"${SCP[@]}" -q -r "$PKG" "sb-target:C:/deploy/$NAME"

info "Installing"
vm_ps sb-target "
  \$ErrorActionPreference = 'Stop'
  Set-Location C:\\deploy\\$NAME
  # Trust whatever test certificate signed the catalog, so PnP accepts the package without a prompt.
  # (Taken from the .cat itself: works whether or not the build put a .cer in the package.)
  Get-ChildItem *.cat | ForEach-Object {
    \$cert = (Get-AuthenticodeSignature \$_.FullName).SignerCertificate
    if (-not \$cert) { throw \"\$(\$_.Name) is not signed\" }
    foreach (\$storeName in 'Root', 'TrustedPublisher') {
      \$store = New-Object System.Security.Cryptography.X509Certificates.X509Store \$storeName, 'LocalMachine'
      \$store.Open('ReadWrite'); \$store.Add(\$cert); \$store.Close()
    }
    Write-Host \"trusted signer: \$(\$cert.Subject)\"
  }
  # Create the root device first: pnputil /install only binds to devices that already exist.
  if ('$HWID') {
    C:\\tools\\devgen.exe /add /bus ROOT /hardwareid '$HWID'
    if (\$LASTEXITCODE) { throw 'devgen failed' }
  }
  pnputil /add-driver *.inf /install
  if (\$LASTEXITCODE -notin 0, 259, 3010) { throw \"pnputil failed (\$LASTEXITCODE)\" }"

if (( VERIFIER )) && [[ -n $SYS ]]; then
  info "Enabling Driver Verifier on: $SYS (reboots)"
  vm_ps sb-target "verifier /standard /driver $SYS | Out-Null; Restart-Computer -Force" || true
  sleep 20; wait_ssh target 600
  vm_ps sb-target "verifier /querysettings" | sed -n '1,15p'
fi

info "Result"
vm_ps sb-target "
  pnputil /enum-drivers | Select-String -Context 0,6 -SimpleMatch '$(ls "$PKG"/*.inf | head -1 | xargs basename)'
  if ('$HWID') {
    Get-PnpDevice | Where-Object { \$_.HardwareID -contains '$HWID' } |
      Format-Table -AutoSize Status, Class, FriendlyName, InstanceId, @{n='Problem';e={\$_.Problem}}
  }
  Get-WinEvent -FilterHashtable @{LogName='System'; Level=1,2; StartTime=(Get-Date).AddMinutes(-10)} -MaxEvents 10 -ErrorAction SilentlyContinue |
    Format-Table -AutoSize TimeCreated, ProviderName, Message"
