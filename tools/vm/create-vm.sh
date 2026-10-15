#!/usr/bin/env bash
# Create a VM and install Windows 11 Pro unattended, ending reachable over SSH.
#   tools/vm/create-vm.sh build|target [--iso path/to/Win11.iso] [--force]
# Takes ~20-40 min per VM. --force deletes an existing VM of that name first.
set -euo pipefail
source "$(dirname "$0")/config.sh"

ROLE=${1:-}; NAME=$(vm_name "$ROLE"); shift
WIN_ISO=""; FORCE=0
while (( $# )); do
  case $1 in
    --iso) WIN_ISO=$2; shift 2 ;;
    --force) FORCE=1; shift ;;
    *) die "unknown option $1" ;;
  esac
done

id -nG | grep -qw libvirt || die "you're not in the libvirt group yet: run host-setup.sh, then log out and back in"
[[ -w $POOL_DIR ]] || die "$POOL_DIR missing or not writable: run sudo bash tools/vm/host-setup.sh"

# --- one-time state: VM password + SSH key ------------------------------------
mkdir -p "$STATE_DIR"; chmod 700 "$STATE_DIR"
[[ -f $STATE_DIR/password ]] || { head -c 18 /dev/urandom | base64 | tr -d '/+=' > "$STATE_DIR/password"; chmod 600 "$STATE_DIR/password"; }
[[ -f $SSH_KEY ]] || ssh-keygen -q -t ed25519 -N '' -C "sb-vm" -f "$SSH_KEY"
PASSWORD=$(cat "$STATE_DIR/password")

# --- ISOs into the pool (libvirt can't read $HOME) ----------------------------
if [[ -z $WIN_ISO ]]; then
  # Microsoft names it Win11_<ver>_<lang>_x64.iso or Windows11_Client_x64_<lang>_<build>.iso depending on the source.
  WIN_ISO=$(find "$POOL_DIR" ~/Downloads -maxdepth 1 -name 'Win*11*.iso' -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -1 | cut -d' ' -f2-)
  [[ -n $WIN_ISO ]] || die "no Windows 11 ISO (Win*11*.iso) found in ~/Downloads; download it from microsoft.com/software-download/windows11 or pass --iso"
fi
[[ -f $WIN_ISO ]] || die "ISO not found: $WIN_ISO"
stage() {  # copy into the pool unless an identical-size copy is already there
  local src=$1 dst=$POOL_DIR/$(basename "$1")
  if [[ ! -f $dst || $(stat -c %s "$src") != $(stat -c %s "$dst") ]]; then
    info "Copying $(basename "$src") to $POOL_DIR"; cp "$src" "$dst"
  fi
  echo "$dst"
}
WIN_ISO=$(stage "$WIN_ISO")
VIRTIO_ISO=$(stage "${VIRTIO_ISO:-$HOME/Downloads/virtio-win.iso}")

# --- unattend CD: answer file + first-logon script + our public key ----------
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
sed -e "s|@COMPUTER_NAME@|${NAME^^}|" -e "s|@PASSWORD@|$PASSWORD|g" \
  "$VM_DIR/unattend/autounattend.xml.in" > "$work/autounattend.xml"
cp "$VM_DIR/unattend/firstlogon.ps1" "$work/"
cp "$SSH_KEY.pub" "$work/authorized_keys"
UNATTEND_ISO=$POOL_DIR/unattend-$ROLE.iso
genisoimage -quiet -J -r -V UNATTEND -o "$UNATTEND_ISO" "$work"

# --- (re)create the VM -----------------------------------------------------------
DISK=$POOL_DIR/$NAME.qcow2
if virsh dominfo "$NAME" &>/dev/null; then
  (( FORCE )) || die "$NAME already exists (use --force to delete and recreate it)"
  info "Deleting existing $NAME"
  virsh destroy "$NAME" &>/dev/null || true
  virsh undefine "$NAME" --nvram --tpm --checkpoints-metadata --snapshots-metadata
  rm -f "$DISK" "$POOL_DIR/$NAME"-*.qcow2
fi
rm -f "$DISK"
ssh-keygen -q -R "${VM_IP[$ROLE]}" -f "$STATE_DIR/known_hosts" &>/dev/null || true  # new install, new host key

# sb-build runs the engine's WASAPI tests: two sound cards give two independently clocked
# outputs (HDA "Speakers" + USB "Speakers (Audio Device)") and a capture device, for drift tests.
EXTRA=()
[[ $ROLE == build ]] && EXTRA+=(--sound model=ich9 --sound model=usb)

info "Creating $NAME: ${VM_VCPUS[$ROLE]} vCPU, $((VM_RAM[$ROLE] / 1024)) GiB RAM, ${VM_DISK[$ROLE]} GiB disk, ${VM_NIC[$ROLE]} NIC"
virt-install \
  --name "$NAME" --osinfo win11 \
  --vcpus "${VM_VCPUS[$ROLE]}" --memory "${VM_RAM[$ROLE]}" \
  --cpu host-passthrough \
  --boot uefi,firmware.feature0.name=secure-boot,firmware.feature0.enabled=yes,firmware.feature1.name=enrolled-keys,firmware.feature1.enabled=yes \
  --tpm emulator,model=tpm-crb,version=2.0 \
  --disk path="$WIN_ISO",device=cdrom,bus=sata,boot.order=1 \
  --disk path="$DISK",size="${VM_DISK[$ROLE]}",format=qcow2,bus=virtio,discard=unmap,boot.order=2 \
  --disk path="$VIRTIO_ISO",device=cdrom,bus=sata \
  --disk path="$UNATTEND_ISO",device=cdrom,bus=sata \
  --network network="$NET_NAME",model="${VM_NIC[$ROLE]}",mac="${VM_MAC[$ROLE]}" \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --graphics spice --video qxl \
  "${EXTRA[@]}" \
  --install no_install=yes \
  --noautoconsole

# The Windows CD waits a few seconds for "Press any key to boot from CD". Press it.
for _ in $(seq 30); do virsh send-key "$NAME" KEY_ENTER &>/dev/null || true; sleep 1; done

info "Windows is installing unattended. Watch it with: virt-viewer -c qemu:///system $NAME"
info "Waiting for SSH (up to 90 min)..."
wait_ssh "$ROLE" 5400
# sshd starts a moment before first-logon writes its marker, so give it time.
for _ in $(seq 60); do
  [[ $(vm_ps "$NAME" 'Test-Path C:\sb-firstlogon.done' 2>/dev/null) == True ]] && break
  sleep 10
done
[[ $(vm_ps "$NAME" 'Test-Path C:\sb-firstlogon.done' 2>/dev/null) == True ]] \
  || die "SSH is up but first-logon didn't finish; check C:\\sb-firstlogon.log in the VM"

# Done with the install media. The unattend CD holds the password, so don't leave it attached.
for dev in $(virsh domblklist "$NAME" --details | awk '$2=="cdrom"{print $3}'); do
  virsh change-media "$NAME" "$dev" --eject --config --live &>/dev/null || true
done
rm -f "$UNATTEND_ISO"

info "$NAME is up at ${VM_IP[$ROLE]}. Next: tools/vm/provision.sh $ROLE"
