# Spike 3: dynamic ACX endpoints

A throwaway C experiment (2026-11-18). It answers whether an ACX driver can create and destroy
Windows audio endpoints at runtime, how they get their names, and what Windows and apps do
around that. It's a reference for the Rust driver's D3–D7, not part of the product.

**Result:** it works. A child device created at runtime becomes a real Windows endpoint in about
1–2 s, takes the name the driver gives it, keeps its endpoint ID when its instance ID is reused,
and can be removed while apps play into it. Full write-up: *Driver Notes → Spike 3* in the
project notes.

## What's here

| Path | What |
|---|---|
| `sample/` | Microsoft's ACX **AudioCodec** sample ([Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples), `audio/Acx/Samples`, MS-PL: see `sample/LICENSE`), modified as below |
| `observe.ps1` | Run on `sb-target`: restarts the root device, records every change in the spike's devices and Windows' audio endpoints, then dumps the driver's log and Windows' endpoint registry |
| `q45.ps1` | Run on `sb-target`: removal while two apps play into the endpoint, and whether a per-app route survives the endpoint being re-created (needs `player.ps1`, plus `app_output.exe` and `default_output.exe` built with a static CRT, see below) |
| `player.ps1` | A WASAPI player in C# (through `Add-Type`): plays silence to one endpoint or to the process's default, and logs every error |
| `ifaces.ps1`, `devprops.ps1` | Run on `sb-target`: KS interface registrations and properties, and a child's device properties next to the root device's |

## Changes to the sample

- **`AudioCodec/Driver/Device.cpp`**
  - The static speaker is gone.
  - A timer on the root device creates and destroys a render child in rounds. The child is a raw PDO (`WdfPdoInitAllocate`, `WdfPdoInitAssignRawDevice`, `AcxDeviceInitConfigRawDevice`, `AcxDeviceAddCircuitDevice`, then `AcxDeviceRemoveCircuitDevice`). The current rounds (`g_SpikeSteps`) are the Q4/Q5 ones: create, 60 s, remove, 15 s, re-create with the same instance ID.
  - Each child creates its circuit in its own `PrepareHardware` and names itself with `DEVPKEY_Device_FriendlyName`.
  - Every step goes as a `Log###` value under the root device's `Device Parameters` key.
  - Idle power-down is off.
  - The fix for a bug in the sample: the device cleanup touched the static mic's circuit after `AcxDeviceRemoveCircuit` had deleted it (bugcheck 0x7E under Driver Verifier on removal).
- **`AudioCodec/Driver/AudioCodec.inf`** (UTF-16): a model for the child's hardware ID `SoundboardSpike\Endpoint` with a **null service** and the KS interfaces. Without it, the child never becomes a Windows endpoint.
- **`Common/RenderCircuit.cpp`**: the bridge pin answers `EvtAcxPinRetrieveName` ("SpikePin rN"), and stream creation is logged.
- **`Common/StreamEngine.cpp`**: stream run and pause are logged.
- **`Shared/Public.h`**: the shared spike declarations.

## Running it

From the repo root on the Linux host:

```bash
# build on sb-build
tar czf target/spike3.tgz -C tools/spikes/spike3/sample .
scp -F tools/vm/ssh_config target/spike3.tgz sb-build:C:/src/spike3.tgz
tools/vm/vm.sh ssh build 'Remove-Item -Recurse -Force C:\src\spike3 -EA 0; mkdir C:\src\spike3 | Out-Null; tar -xzf C:\src\spike3.tgz -C C:\src\spike3'
#   then msbuild C:\src\spike3\AudioCodec\Driver\AudioCodec.sln /p:Configuration=Debug /p:Platform=x64
#   (the same MSBuild that tools/vm/smoke-test.sh finds with vswhere)

# deploy to a clean sb-target, under Driver Verifier
scp -F tools/vm/ssh_config -r 'sb-build:C:/src/spike3/AudioCodec/Driver/x64/Debug/AudioCodec' target/vm/spike3
tools/vm/deploy-driver.sh target/vm/spike3 --hwid 'ROOT\AudioCodec' --verifier

# observe
scp -F tools/vm/ssh_config tools/spikes/spike3/*.ps1 sb-target:C:/deploy/
tools/vm/vm.sh ssh target 'powershell -ExecutionPolicy Bypass -File C:\deploy\observe.ps1 -Seconds 130 -RestartParent; Get-Content C:\deploy\observe.log'
```

`q45.ps1` also needs the controller's `app_output` and `default_output` examples, because
`sb-target` has no VC runtime. Build them with a static CRT on `sb-build`:
`$env:RUSTFLAGS='-C target-feature=+crt-static'; cargo build --release -p sb-controller --example app_output --example default_output --target-dir target\static`.
Then copy both `.exe` files to `C:\deploy` on the target.

The rounds start 20 s after the root device starts. `observe.ps1 -RestartParent` clears the old
log and restarts the root device, which unloads and reloads the driver, so the rounds begin again
from the first.
