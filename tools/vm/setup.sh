#!/usr/bin/env bash
# Build the whole test environment from scratch (after host-setup.sh). ~1.5-3 h, mostly unattended.
#   tools/vm/setup.sh [--iso path/to/Win11.iso]
set -euo pipefail
source "$(dirname "$0")/config.sh"

"$VM_DIR/create-vm.sh" build "$@"
"$VM_DIR/provision.sh" build
"$VM_DIR/create-vm.sh" target "$@"
"$VM_DIR/provision.sh" target      # ends by sealing the clean baseline
"$VM_DIR/vm.sh" status
info "Environment ready. Try: tools/vm/smoke-test.sh"
