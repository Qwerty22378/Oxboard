#!/usr/bin/env bash
# Day-to-day VM control.
#   vm.sh status                 both VMs: state + SSH reachability
#   vm.sh start|stop build|target|all
#   vm.sh ssh build|target [cmd] interactive shell, or run a PowerShell command
#   vm.sh view build|target      open the graphical console
#   vm.sh seal                   target: current disk becomes the clean baseline
#   vm.sh revert                 target: throw away all changes since seal, boot clean
#   vm.sh kd                     attach the kernel debugger (kd.exe on sb-build) to sb-target
#   vm.sh password               print the Windows password for user 'dev'
set -euo pipefail
source "$(dirname "$0")/config.sh"

TARGET=sb-target
DISK=$POOL_DIR/$TARGET.qcow2
BASE=$POOL_DIR/$TARGET-clean.qcow2

new_overlay() {  # fresh empty overlay on the baseline, created by libvirt (root) via the pool
  virsh vol-delete --pool "$POOL_NAME" "$(basename "$DISK")" &>/dev/null || true
  virsh vol-create-as "$POOL_NAME" "$(basename "$DISK")" "${VM_DISK[target]}G" --format qcow2 \
    --backing-vol "$(basename "$BASE")" --backing-vol-format qcow2 >/dev/null
}

roles() { [[ ${1:-} == all ]] && echo "build target" || { vm_name "${1:-}" >/dev/null; echo "$1"; }; }

stop_vm() {  # graceful shutdown, hard stop after 2 min
  local name=$1
  [[ $(virsh domstate "$name") == "shut off" ]] && return
  virsh shutdown "$name" >/dev/null
  for _ in $(seq 24); do [[ $(virsh domstate "$name") == "shut off" ]] && return; sleep 5; done
  echo "$name didn't shut down in 2 min, forcing off"; virsh destroy "$name" >/dev/null
}

case "${1:-}" in
status)
  for r in build target; do
    n=sb-$r; st="not created"
    virsh dominfo "$n" &>/dev/null && st=$(virsh domstate "$n" | head -1)
    ssh_ok=no; [[ $st == running ]] && "${SSH[@]}" -o ConnectTimeout=3 -o BatchMode=yes "$n" exit 2>/dev/null && ssh_ok=yes
    printf '%-10s %-12s ssh:%-4s %s\n' "$n" "$st" "$ssh_ok" "${VM_IP[$r]}"
  done
  [[ -f $BASE ]] && echo "target baseline: $(date -r "$BASE" '+%F %H:%M')" || echo "target baseline: none (run: vm.sh seal)"
  ;;
start) for r in $(roles "${2:-}"); do virsh domstate "sb-$r" | grep -q running || virsh start "sb-$r"; done ;;
stop)  for r in $(roles "${2:-}"); do stop_vm "sb-$r"; done ;;
ssh)
  n=$(vm_name "${2:-}")
  if (( $# > 2 )); then vm_ps "$n" "${*:3}"; else exec "${SSH[@]}" "$n"; fi
  ;;
view) exec virt-viewer -c qemu:///system "$(vm_name "${2:-}")" ;;
password) cat "$STATE_DIR/password"; echo ;;

seal)
  # Baseline = a qcow2 the VM never writes to; it always runs on a throwaway overlay on top of it.
  # Disks are root-owned (libvirt made them), so all disk changes go through the libvirt pool.
  stop_vm "$TARGET"
  if [[ -f $BASE ]]; then
    read -rp "Replace the existing baseline from $(date -r "$BASE" '+%F %H:%M') with the current state? [y/N] " a
    [[ $a == [yY] ]] || exit 1
    # Fold the overlay into the baseline. blockcommit needs a live (here: paused) domain.
    virsh start "$TARGET" --paused >/dev/null
    virsh blockcommit "$TARGET" vda --active --pivot --wait >/dev/null
    virsh destroy "$TARGET" >/dev/null
  else
    mv "$DISK" "$BASE"                  # the pool directory is ours, so a rename works even on root's files
    virsh pool-refresh "$POOL_NAME" >/dev/null
  fi
  new_overlay
  echo "Sealed. 'vm.sh revert' now returns sb-target to this state."
  ;;
revert)
  [[ -f $BASE ]] || die "no baseline yet; run 'vm.sh seal' first"
  virsh destroy "$TARGET" &>/dev/null || true
  new_overlay
  virsh start "$TARGET" >/dev/null
  echo "sb-target reverted to the baseline, booting..."
  wait_ssh target 300
  echo "sb-target is up."
  ;;

kd)
  key=$(cat "$STATE_DIR/kdkey" 2>/dev/null) || die "no KDNET key; provision the target first"
  echo "Waiting for sb-target to connect (it does on boot; 'vm.sh revert' or a reboot triggers it)."
  echo "In kd: Ctrl+C breaks in, 'g' continues, 'q' quits. Symbols come from the Microsoft symbol server."
  ps="& \"\${env:ProgramFiles(x86)}\\Windows Kits\\10\\Debuggers\\x64\\kd.exe\" -y 'srv*C:\\symbols*https://msdl.microsoft.com/download/symbols' -k net:port=$KD_PORT,key=$key"
  exec "${SSH[@]}" -t sb-build "powershell -NoProfile -EncodedCommand $(printf '%s' "$ps" | iconv -t UTF-16LE | base64 -w0)"
  ;;
*) sed -n '2,11p' "$0"; exit 2 ;;
esac
