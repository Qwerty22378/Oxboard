param([string]$Match = 'SoundboardSpike#Endpoint#1&49a4548&0&2|ROOT#DEVGEN')
$names = @{ '{6994ad04-93ef-11d0-a3cc-00a0c9223196}'='AUDIO'; '{65e8773e-8f56-11d0-a3b9-00a0c9223196}'='RENDER'; '{65e8773d-8f56-11d0-a3b9-00a0c9223196}'='CAPTURE'; '{eb115ffc-10c8-4964-831d-6dcb02e6f23f}'='REALTIME'; '{2c6bb644-e1ae-47f8-9a2b-1d1fa750f2fa}'='ACXCIRCUIT?' }
foreach ($g in $names.Keys) {
  Get-ChildItem "HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\$g" -ErrorAction SilentlyContinue |
    Where-Object { $_.PSChildName -match $Match } | ForEach-Object {
      $dev = $_.PSChildName -replace '^##\?#','' -replace '#\{.*$',''
      Get-ChildItem -LiteralPath $_.PSPath | ForEach-Object {
        $ref = $_.PSChildName
        $linked = (Get-ItemProperty -LiteralPath (Join-Path $_.PSPath 'Control') -ErrorAction SilentlyContinue).Linked
        $p = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
        $ep = Test-Path -LiteralPath (Join-Path $_.PSPath 'Device Parameters\EP\0')
        '{0,-12} {1,-12} linked={2} clsid={3} fn={4} EP\0(params)={5}  {6}' -f $names[$g], $ref, $linked, ($p.CLSID -ne $null), $p.FriendlyName, $ep, $dev
        $dp = Get-ItemProperty -LiteralPath (Join-Path $_.PSPath 'Device Parameters') -ErrorAction SilentlyContinue
        if ($dp) { '             Device Parameters: ' + (($dp.PSObject.Properties | ? { $_.Name -notlike 'PS*' } | % { "$($_.Name)=$($_.Value)" }) -join '; ') }
      }
    }
}
