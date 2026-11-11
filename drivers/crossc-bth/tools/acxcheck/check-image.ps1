# Post-link check of the ACX class-extension binding in crossc_bth (no elevation,
# nothing is loaded). Run after `cargo make` / `cargo make default --release`:
#
#   powershell -ExecutionPolicy Bypass -File tools\acxcheck\check-image.ps1
#
# The ACX function table lives in the driver image and is filled by the WDF
# loader through _Acx_BIND_INFO in section .kmdfclassbind$b. If the linker drops
# that section the table stays empty and the driver runs without ACX
# (src/acx.rs refuses to call through an unbound table). This script fails if
#   * .kmdfclassbind$b is missing or not 0x78 bytes,
#   * _Acx_BIND_INFO / AcxFunctions / AcxDriverGlobals / AcxMinimumVersionRequired
#     are missing from the map,
#   * WdfMinimumVersionRequired does not come from wdk-sys (acxstub.lib carries
#     its own copy with KMDF minor 31; build.rs orders the link so the wdk-sys
#     one, minor 33, wins),
#   * the image imports anything but ntoskrnl / HAL / WDFLDR (ACX DDIs are table
#     calls, never imports).
param([string[]]$Profiles = @('debug', 'release'))
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$bad = 0
foreach ($p in $Profiles) {
    $map = Join-Path $root "target\$p\deps\crossc_bth.map"
    $sys = Join-Path $root "target\$p\crossc_bth_package\crossc_bth.sys"
    if (-not (Test-Path $map) -or -not (Test-Path $sys)) { Write-Host "[$p] not built, skipped"; continue }
    $m = Get-Content $map
    $fail = @()
    if (-not ($m -match '\s00000078H\s+\.kmdfclassbind\$b\s')) { $fail += '.kmdfclassbind$b (0x78 bytes) missing' }
    foreach ($s in '_Acx_BIND_INFO', 'AcxFunctions', 'AcxDriverGlobals', 'AcxMinimumVersionRequired') {
        if (-not ($m -match "\s$s\s+[0-9a-f]{16}\s")) { $fail += "$s missing from map" }
    }
    $w = $m -match '\sWdfMinimumVersionRequired\s+[0-9a-f]{16}\s'
    if (-not $w) { $fail += 'WdfMinimumVersionRequired missing from map' }
    elseif ($w -notmatch 'libwdk_sys') { $fail += "WdfMinimumVersionRequired is not the wdk-sys copy: $w" }

    $dumpbin = Get-ChildItem "$env:ProgramFiles\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -Last 1
    if ($dumpbin) {
        $dlls = & $dumpbin.FullName /nologo /imports $sys | Where-Object { $_ -match '^\s{4}\S+\.(exe|sys|dll)\s*$' } | ForEach-Object { $_.Trim().ToLower() }
        foreach ($d in $dlls) {
            if ($d -notin 'ntoskrnl.exe', 'hal.dll', 'wdfldr.sys') { $fail += "unexpected import from $d" }
        }
        Write-Host "[$p] imports: $($dlls -join ', ')"
    } else {
        Write-Host "[$p] dumpbin.exe not found, import check skipped"
    }
    if ($fail) { $bad++; $fail | ForEach-Object { Write-Host "[$p] FAIL: $_" } }
    else { Write-Host "[$p] OK: ACX bind info present, WdfMinimumVersionRequired from wdk-sys" }
}
exit $bad
