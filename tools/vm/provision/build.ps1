# Provision sb-build: driver toolchain + debugger. Run over SSH by provision.sh; safe to re-run.
#   VS 2022 Build Tools (MSVC + Spectre libs), Windows SDK + WDK, LLVM (bindgen), Rust + cargo-make, Git, WinDbg.
param([int]$KdPort = 50000)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Kit = '10.0.26100'   # SDK and WDK versions must match

function Install-Pkg([string]$Id, [string[]]$Extra = @()) {
    Write-Host ">>> $Id"
    winget.exe install --source winget --exact --id $Id --silent --disable-interactivity `
        --accept-source-agreements --accept-package-agreements @Extra
    # 0 = installed; these two mean "already installed / nothing newer", fine on re-runs.
    if ($LASTEXITCODE -notin 0, -1978335189, -1978335135) { throw "winget $Id failed ($LASTEXITCODE)" }
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
    Add-AppxPackage -RegisterByFamilyName -MainPackage Microsoft.DesktopAppInstaller_8wekyb3d8bbwe
}

Install-Pkg Microsoft.VisualStudio.2022.BuildTools @('--override', (@(
    '--quiet --wait --norestart --nocache',
    '--add Microsoft.VisualStudio.Workload.VCTools',
    '--add Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
    '--add Microsoft.VisualStudio.Component.VC.Runtimes.x86.x64.Spectre',
    '--add Microsoft.VisualStudio.Component.Windows11SDK.26100',
    '--includeRecommended') -join ' '))
Install-Pkg "Microsoft.WindowsSDK.$Kit"
Install-Pkg "Microsoft.WindowsWDK.$Kit"
Install-Pkg LLVM.LLVM
Install-Pkg Rustlang.Rustup
Install-Pkg Git.Git
Install-Pkg Microsoft.WinDbg

# WDK MSBuild integration (WindowsKernelModeDriver10.0 toolset), needed for Microsoft's C samples, not
# the Rust driver. WDK 26100 ships it as a VS installer component, not a .vsix. `modify` is idempotent.
Write-Host ">>> WDK MSBuild integration for Build Tools"
$vsSetup = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe"
$btPath = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools"
Start-Process $vsSetup -Wait -ArgumentList 'modify', '--installPath', "`"$btPath`"",
    '--add', 'Component.Microsoft.Windows.DriverKit.BuildTools', '--quiet', '--norestart', '--nocache'

# bindgen finds libclang through LIBCLANG_PATH.
[Environment]::SetEnvironmentVariable('LIBCLANG_PATH', "$env:ProgramFiles\LLVM\bin", 'Machine')
$env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"

$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
$rustup = "$env:USERPROFILE\.cargo\bin\rustup.exe"
& $rustup default stable
& $rustup component add rust-src
if (-not (Test-Path "$env:USERPROFILE\.cargo\bin\cargo-make.exe")) {
    & $cargo install --locked cargo-make --no-default-features --features tls-native
    if ($LASTEXITCODE) { throw "cargo install cargo-make failed" }
}

# Builds: keep Defender out of the source tree and toolchain (big speedup), allow long paths.
Add-MpPreference -ExclusionPath 'C:\src', "$env:USERPROFILE\.cargo", "$env:USERPROFILE\.rustup"
New-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem -Name LongPathsEnabled -Value 1 -PropertyType DWord -Force | Out-Null
New-Item -ItemType Directory -Force C:\src | Out-Null

# The kernel debugger listens here for KDNET from sb-target.
if (-not (Get-NetFirewallRule -Name SB-KDNET-In -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name SB-KDNET-In -DisplayName "KDNET from sb-target (UDP $KdPort)" `
        -Protocol UDP -LocalPort $KdPort -Action Allow -Direction Inbound | Out-Null
}
# Updates: download automatically, but never reboot while the dev user is logged on.
$wu = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU'
New-Item $wu -Force | Out-Null
New-ItemProperty $wu -Name NoAutoRebootWithLoggedOnUsers -Value 1 -PropertyType DWord -Force | Out-Null

# --- verify ---------------------------------------------------------------------
$kits = "${env:ProgramFiles(x86)}\Windows Kits\10"
$checks = [ordered]@{
    'MSVC (cl.exe)'       = [bool](Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe" -ErrorAction SilentlyContinue)
    'WDK headers (wdm.h)' = Test-Path "$kits\Include\$Kit.0\km\wdm.h"
    'ACX headers'         = [bool](Get-ChildItem "$kits\Include\$Kit.0\km\acx" -ErrorAction SilentlyContinue)
    'libclang'            = Test-Path "$env:ProgramFiles\LLVM\bin\libclang.dll"
    'cargo-make'          = Test-Path "$env:USERPROFILE\.cargo\bin\cargo-make.exe"
    'kd.exe'              = Test-Path "$kits\Debuggers\x64\kd.exe"
    'devgen.exe'          = [bool](Get-ChildItem "$kits\Tools\*\x64\devgen.exe" -ErrorAction SilentlyContinue)
    'WDK MSBuild toolset' = [bool](Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\MSBuild\Microsoft\VC\*\Platforms\x64\PlatformToolsets\WindowsKernelModeDriver10.0" -ErrorAction SilentlyContinue)
}
$checks.GetEnumerator() | ForEach-Object { '{0,-22} {1}' -f $_.Key, $(if ($_.Value) { 'OK' } else { 'MISSING' }) }
& $cargo --version
if ($checks.Values -contains $false) { Write-Warning "Some components are missing (see above)." }
