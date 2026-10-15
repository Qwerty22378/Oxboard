# Shared settings for the driver test VMs. Sourced by the other scripts.
# shellcheck shell=bash

VM_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$VM_DIR/../.." && pwd)
STATE_DIR=$VM_DIR/.state                 # generated: password, ssh key, unattend ISOs (gitignored)
POOL_DIR=/var/lib/libvirt/images/sb      # disks + ISOs (libvirt can't read $HOME)
NET_NAME=sb-net
POOL_NAME=sb                             # libvirt storage pool over POOL_DIR
export LIBVIRT_DEFAULT_URI=qemu:///system

WIN_USER=dev
SSH_KEY=$STATE_DIR/id_ed25519
SSH=(ssh -F "$VM_DIR/ssh_config")
SCP=(scp -F "$VM_DIR/ssh_config")

declare -A VM_VCPUS=([build]=6     [target]=4)
declare -A VM_RAM=(  [build]=8192  [target]=6144)
declare -A VM_DISK=( [build]=150   [target]=80)
declare -A VM_MAC=(  [build]=52:54:00:5b:00:10 [target]=52:54:00:5b:00:20)
declare -A VM_IP=(   [build]=192.168.150.10    [target]=192.168.150.20)
# Target uses e1000e: KDNET doesn't support virtio-net. Build uses virtio for speed.
declare -A VM_NIC=(  [build]=virtio [target]=e1000e)
# Both install with Secure Boot on (what Windows 11 setup expects). provision.sh target then
# turns it off: testsigning and kernel debugging are blocked under Secure Boot.

KD_PORT=50000

die()  { echo "ERROR: $*" >&2; exit 1; }
# set -e exits silently; say where and what failed instead.
set -E
trap 'rc=$?; echo "ERROR: $(basename "${BASH_SOURCE[0]:-$0}"):$LINENO: \`$BASH_COMMAND\` failed (exit $rc)" >&2' ERR
info() { echo ">>> $*" >&2; }   # stderr, so $(...) captures never pick it up

if [[ $EUID -eq 0 && $(basename "$0") != host-setup.sh ]]; then
  die "don't run $(basename "$0") with sudo; only host-setup.sh needs root. Run it as your normal user."
fi

vm_name() {
  case "${1:-}" in build|target) echo "sb-$1" ;; *) die "expected 'build' or 'target', got '${1:-}'" ;; esac
}

# Run a PowerShell snippet on a VM. Sent base64 (-EncodedCommand) so quotes, $ and
# backslashes survive Windows OpenSSH untouched.   vm_ps sb-build 'Get-Date'
vm_ps() {
  local host=$1 script="\$ProgressPreference = 'SilentlyContinue'; $2"   # progress output arrives as CLIXML noise
  "${SSH[@]}" "$host" "powershell -NoProfile -NonInteractive -EncodedCommand $(printf '%s' "$script" | iconv -t UTF-16LE | base64 -w0)" | tr -d '\r'
}

# Wait until the VM answers on SSH (or time out).
wait_ssh() {
  local role=$1 timeout=${2:-600} start=$SECONDS
  until "${SSH[@]}" -o ConnectTimeout=5 -o BatchMode=yes "sb-$role" "exit 0" 2>/dev/null; do
    (( SECONDS - start < timeout )) || die "sb-$role not reachable over SSH after ${timeout}s"
    sleep 10
  done
}

REMOTE_SRC='C:\src\soundboard'   # working-tree copy on sb-build

# Copy the working tree (tracked + untracked, minus .gitignore'd) to $REMOTE_SRC on sb-build.
# Everything except the build cache (target/) is replaced, so deleted files don't linger.
sync_tree() {
  info "Syncing working tree to sb-build"
  local tgz; tgz=$(mktemp --suffix=.tgz)
  git -C "$REPO_DIR" ls-files -z --cached --others --exclude-standard | tar -C "$REPO_DIR" --null -T - -czf "$tgz"
  "${SCP[@]}" -q "$tgz" sb-build:C:/src/soundboard.tgz
  rm -f "$tgz"
  vm_ps sb-build "
    New-Item -ItemType Directory -Force '$REMOTE_SRC' | Out-Null
    Get-ChildItem '$REMOTE_SRC' -Force | Where-Object Name -ne 'target' | Remove-Item -Recurse -Force
    tar -xzf C:\\src\\soundboard.tgz -C '$REMOTE_SRC'
    if (\$LASTEXITCODE) { exit 1 }"
}
