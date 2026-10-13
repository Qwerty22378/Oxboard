#!/usr/bin/env bash
# One-time host setup (root): KVM/libvirt packages, the sb-net network, and the
# VM storage directory. Safe to re-run.   sudo bash tools/vm/host-setup.sh
set -euo pipefail
source "$(dirname "$0")/config.sh"
[[ $EUID -eq 0 ]] || die "run with sudo"
USER_NAME=${SUDO_USER:?run via sudo from your normal account}

info "Installing packages"
apt-get update -qq
apt-get install -y qemu-system-x86 qemu-utils libvirt-daemon-system libvirt-clients \
  virtinst virt-manager virt-viewer swtpm swtpm-tools ovmf genisoimage

info "Groups: adding $USER_NAME to libvirt + kvm (log out/in, or use 'newgrp libvirt')"
usermod -aG libvirt,kvm "$USER_NAME"
systemctl enable --now libvirtd

info "Storage: $POOL_DIR (owned by $USER_NAME so scripts can create/revert disks without sudo)"
install -d -o "$USER_NAME" -g libvirt -m 2775 "$POOL_DIR"

info "Storage pool: $POOL_NAME"
virsh pool-info "$POOL_NAME" &>/dev/null || virsh pool-define-as "$POOL_NAME" dir --target "$POOL_DIR"
virsh pool-autostart "$POOL_NAME"
virsh pool-info "$POOL_NAME" | grep -q 'State:.*running' || virsh pool-start "$POOL_NAME"

info "Network: $NET_NAME"
if ! virsh net-info "$NET_NAME" &>/dev/null; then
  virsh net-define "$VM_DIR/sb-net.xml"
fi
virsh net-autostart "$NET_NAME"
virsh net-info "$NET_NAME" | grep -q 'Active:.*yes' || virsh net-start "$NET_NAME"

[[ $(cat /sys/module/kvm_amd/parameters/nested 2>/dev/null || cat /sys/module/kvm_intel/parameters/nested) =~ ^(1|Y)$ ]] \
  || echo "WARNING: nested virtualization is off; Memory Integrity (HVCI) inside the target VM won't work."

info "Done. Now log out and back in (for the libvirt group), then run: tools/vm/setup.sh"
