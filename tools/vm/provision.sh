#!/usr/bin/env bash
# Provision a freshly created VM over SSH.   tools/vm/provision.sh build|target
#   build : toolchain (VS Build Tools, SDK/WDK, LLVM, Rust, cargo-make, WinDbg). ~30-60 min.
#   target: updates off, crash dumps, HVCI, Secure Boot OFF, testsigning, KDNET -> sb-build,
#           devgen.exe from sb-build; then seals the clean baseline that deploys revert to.
set -euo pipefail
source "$(dirname "$0")/config.sh"
ROLE=${1:-}; NAME=$(vm_name "$ROLE")

run_ps() {  # copy a provisioning script to the VM and run it with arguments
  local host=$1 script=$2; shift 2
  "${SCP[@]}" -q "$VM_DIR/provision/$script" "$host:C:/Windows/Temp/$script"
  "${SSH[@]}" "$host" "powershell -NoProfile -ExecutionPolicy Bypass -File C:\\Windows\\Temp\\$script $*"
}

wait_ssh "$ROLE" 300
case $ROLE in
build)
  run_ps sb-build build.ps1 -KdPort "$KD_PORT"
  ;;

target)
  wait_ssh build 60 || die "sb-build must be provisioned first (devgen.exe and the debugger live there)"

  info "Stage 1: prep"
  run_ps sb-target target.ps1 -Stage prep

  info "Staging devgen.exe from sb-build"
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  devgen=$(vm_ps sb-build '(Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Tools\*\x64\devgen.exe" | Select-Object -Last 1).FullName')
  [[ -n $devgen ]] || die "devgen.exe not found on sb-build"
  "${SCP[@]}" -q "sb-build:${devgen//\\//}" "$tmp/devgen.exe"
  "${SCP[@]}" -q "$tmp/devgen.exe" sb-target:C:/tools/devgen.exe

  info "Turning Secure Boot off (firmware variables reset to the no-keys template)"
  virsh shutdown "$NAME" >/dev/null
  for _ in $(seq 60); do [[ $(virsh domstate "$NAME") == "shut off" ]] && break; sleep 5; done
  [[ $(virsh domstate "$NAME") == "shut off" ]] || die "$NAME didn't shut down"
  # Flip enrolled-keys off and drop the pinned loader/NVRAM template from the keys-enrolled build,
  # so libvirt re-selects matching firmware (OVMF secboot code + empty vars = Secure Boot off).
  virsh dumpxml --inactive "$NAME" | python3 -c '
import sys, xml.etree.ElementTree as ET
dom = ET.parse(sys.stdin); os_ = dom.find("os")
os_.find("firmware/feature[@name=\"enrolled-keys\"]").set("enabled", "no")
for el in os_.findall("loader"): os_.remove(el)
for el in os_.findall("nvram"): os_.remove(el)
# With kernel debugging on, Windows hangs at boot under libvirt'"'"'s Hyper-V enlightenments
# (relaxed/vapic/spinlocks; verified 2026-10-13). The test box runs without them.
feat = dom.find("features")
for el in feat.findall("hyperv"): feat.remove(el)
clock = dom.find("clock")
for t in clock.findall("timer[@name=\"hypervclock\"]"): clock.remove(t)
dom.write(sys.stdout, encoding="unicode")' > "$tmp/target.xml"
  virsh define "$tmp/target.xml" >/dev/null
  virsh start "$NAME" --reset-nvram >/dev/null
  wait_ssh target 600

  info "Stage 2: testsigning + KDNET"
  # KDNET key: four dot-separated base-36 words. Regenerate if missing or malformed.
  grep -qxE '([a-z0-9]{8}\.){3}[a-z0-9]{8}' "$STATE_DIR/kdkey" 2>/dev/null || python3 -c '
import secrets, string
abc = string.ascii_lowercase + string.digits
print(".".join("".join(secrets.choice(abc) for _ in range(8)) for _ in range(4)))' > "$STATE_DIR/kdkey"
  run_ps sb-target target.ps1 -Stage debug -DebugHost "${VM_IP[build]}" -Port "$KD_PORT" -Key "$(cat "$STATE_DIR/kdkey")" || true  # reboots
  sleep 20; wait_ssh target 600

  info "Verifying"
  run_ps sb-target target.ps1 -Stage verify

  "$VM_DIR/vm.sh" seal
  ;;
esac
info "$NAME provisioned."
