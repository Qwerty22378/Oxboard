# Driver test environment

Two Windows 11 Pro VMs on KVM/libvirt, driven entirely from Linux over SSH.

| VM | IP | Role |
|---|---|---|
| `sb-build` | 192.168.150.10 | Persistent dev box: VS 2022 Build Tools, SDK + WDK 10.0.26100, LLVM, Rust + cargo-make, kd/WinDbg. Builds drivers; runs the kernel debugger. |
| `sb-target` | 192.168.150.20 | Throwaway test box: Secure Boot **off**, testsigning **on**, KDNET → `sb-build:50000`, Memory Integrity requested. Runs on an overlay over a sealed baseline; every deploy starts from clean. |

Both are on the `sb-net` NAT network (internet + each other). Disks and ISOs live in `/var/lib/libvirt/images/sb/`.
Generated secrets (VM password, SSH key, KDNET key) live in `tools/vm/.state/` (gitignored).

## First-time setup
1. `sudo bash tools/vm/host-setup.sh`: packages, `sb-net`, storage dir, `libvirt` group. **Log out and back in afterwards.**
2. Download the Windows 11 ISO (English International or English, x64) from
   https://www.microsoft.com/software-download/windows11 into `~/Downloads` (any `Win*11*.iso` name works).
3. `tools/vm/setup.sh`: creates and provisions both VMs, then seals the target baseline. Takes 1.5–3 h, unattended.
   Watch either VM with `tools/vm/vm.sh view build|target`.
4. `tools/vm/smoke-test.sh`: builds Microsoft's C ACX sample, deploys it, checks the device (milestone **D1**).

Steps can be re-run individually: `create-vm.sh <role> [--force]`, `provision.sh <role>`.

## Daily loop
```bash
tools/vm/vm.sh kd                                   # terminal 1: kernel debugger, waits for the target
tools/vm/build-driver.sh                            # terminal 2: sync tree → sb-build, `cargo make` in driver/
tools/vm/deploy-driver.sh target/vm/<pkg> --hwid 'Root\SoundboardVirtualAudio' [--verifier]
```
`deploy-driver.sh` reverts the target to its baseline first (so a BSOD never costs anything), trusts the
package's test cert, installs with `pnputil`, creates the device with `devgen`, and prints device status and recent errors.

User-mode crates (engine, controller) build and run on `sb-build` with `win-cargo.sh`, which syncs the tree
and runs any cargo command there. `sb-build` has an emulated HDA sound card ("Speakers" / "Line In"), so WASAPI code runs end to end:
```bash
tools/vm/win-cargo.sh test -p sb-engine
tools/vm/win-cargo.sh run -p sb-engine --release --example passthrough -- --seconds 60   # E1
tools/vm/win-cargo.sh run -p sb-engine --release --example mix -- --seconds 60           # E2 (--help for options)
```

Other commands: `vm.sh status | start | stop | ssh <role> [cmd] | view <role> | seal | revert | password`.
- `vm.sh revert` – discard everything since the last seal and boot clean (seconds).
- `vm.sh seal` – make the target's current state the new baseline (e.g. after installing a tool you always want).

## Notes and limits
- **Secure Boot is off on the target** because Windows blocks testsigning and kernel debugging under Secure Boot.
  Release testing with real signatures needs a separate Secure Boot VM later.
- **Memory Integrity (HVCI):** requested on the target. `provision.sh target` reports whether it actually runs.
  Windows may refuse VBS without Secure Boot. If it does, use Driver Verifier's code-integrity checks
  (`verifier /flags 0x02000000`) for HVCI compatibility until a signed build can be tested with Secure Boot on.
- **KDNET needs the e1000e NIC** on the target (virtio-net isn't supported). Keep it when editing the VM.
- **The target runs without Hyper-V enlightenments.** With kernel debugging on, Windows hangs at boot under libvirt's
  `relaxed`/`vapic`/`spinlocks` (verified 2026-10-13). `provision.sh target` strips them; don't add them back.
- **Disks are managed through the libvirt pool `sb`.** libvirt creates them root-owned, so `vm.sh seal/revert`
  use `virsh vol-*` / `blockcommit` rather than touching files directly.
- **Diagnosing a VM without SSH:** every VM has a QEMU guest-agent channel, e.g.
  `virsh qemu-agent-command sb-target '{"execute":"guest-ping"}'` (and `guest-exec` to run commands).
- **The target doesn't take Windows updates** (baseline stays stable). The build VM downloads updates but never reboots while you're logged on.
- **The target's firmware variables aren't part of the baseline.** Only the disk reverts, which is fine for driver installs.
- **No guest internet?** Docker's firewall rules can block libvirt NAT. Check with `vm.sh ssh build 'Test-Connection 1.1.1.1 -Count 1'`.
