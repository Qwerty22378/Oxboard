# Provision sb-target. Run over SSH by provision.sh in two stages:
#   -Stage prep   : settings that are fine under Secure Boot (updates off, crash dumps, HVCI request)
#   -Stage debug  : after the host has turned Secure Boot off: testsigning + KDNET to sb-build
param(
    [Parameter(Mandatory)][ValidateSet('prep', 'debug', 'verify')][string]$Stage,
    [string]$DebugHost, [int]$Port = 50000, [string]$Key
)
$ErrorActionPreference = 'Stop'

switch ($Stage) {
'prep' {
    # Snapshot-based box: no Windows Update changing things under us.
    $wu = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU'
    New-Item $wu -Force | Out-Null
    New-ItemProperty $wu -Name NoAutoUpdate -Value 1 -PropertyType DWord -Force | Out-Null

    # Kernel memory dump on bugcheck, keep every dump, reboot afterwards.
    $cc = 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl'
    Set-ItemProperty $cc -Name CrashDumpEnabled -Value 2
    Set-ItemProperty $cc -Name Overwrite -Value 1
    Set-ItemProperty $cc -Name AutoReboot -Value 1
    Set-ItemProperty $cc -Name AlwaysKeepMemoryDump -Value 1

    # Request Memory Integrity (HVCI). Whether it actually runs without Secure Boot is checked in 'verify'.
    $dg = 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceGuard'
    New-Item "$dg\Scenarios\HypervisorEnforcedCodeIntegrity" -Force | Out-Null
    New-ItemProperty $dg -Name EnableVirtualizationBasedSecurity -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty $dg -Name RequirePlatformSecurityFeatures -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty "$dg\Scenarios\HypervisorEnforcedCodeIntegrity" -Name Enabled -Value 1 -PropertyType DWord -Force | Out-Null

    Enable-NetFirewallRule -Name FPS-ICMP4-ERQ-In -ErrorAction SilentlyContinue   # ping, for scripts
    New-Item -ItemType Directory -Force C:\tools, C:\deploy | Out-Null
    Write-Host "prep done"
}

'debug' {
    if (Confirm-SecureBootUEFI) { throw "Secure Boot is still on; testsigning/debug settings would be rejected." }
    if (-not ($DebugHost -and $Key)) { throw "-DebugHost and -Key are required" }

    bcdedit /set testsigning on
    bcdedit /debug on
    bcdedit /dbgsettings net hostip:$DebugHost port:$Port key:$Key | Out-Null
    if ($LASTEXITCODE) { throw "bcdedit /dbgsettings failed" }

    # KDNET drives the NIC directly; tell it which PCI device (the e1000e) to use.
    $nic = Get-NetAdapter -Physical | Where-Object InterfaceDescription -match '82574' | Select-Object -First 1
    if (-not $nic) { throw "e1000e (Intel 82574L) NIC not found" }
    $loc = (Get-PnpDeviceProperty -InstanceId $nic.PnPDeviceID -KeyName DEVPKEY_Device_LocationInfo).Data
    if ($loc -notmatch 'PCI bus (\d+), device (\d+), function (\d+)') { throw "can't parse NIC location '$loc'" }
    bcdedit /set '{dbgsettings}' busparams "$($Matches[1]).$($Matches[2]).$($Matches[3])"
    Write-Host "KDNET -> ${DebugHost}:$Port via NIC at $loc. Rebooting."
    Restart-Computer -Force
}

'verify' {
    $bcd = bcdedit /enum '{current}' | Out-String
    $dbg = bcdedit /dbgsettings | Out-String
    $dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard
    $checks = [ordered]@{
        'Secure Boot off'   = -not (Confirm-SecureBootUEFI)
        'testsigning'       = $bcd -match 'testsigning\s+Yes'
        'kernel debug'      = $bcd -match 'debug\s+Yes'
        'KDNET settings'    = $dbg -match 'debugtype\s+NET'
        'VBS running'       = $dg.VirtualizationBasedSecurityStatus -eq 2
        'HVCI running'      = $dg.SecurityServicesRunning -contains 2
        'devgen.exe staged' = Test-Path C:\tools\devgen.exe
    }
    $checks.GetEnumerator() | ForEach-Object { '{0,-18} {1}' -f $_.Key, $(if ($_.Value) { 'yes' } else { 'NO' }) }
}
}
