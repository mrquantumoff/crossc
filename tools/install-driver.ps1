<#
.SYNOPSIS
    Builds the crossc_bth driver package (and crossc-cli) and installs it on the Galaxy Buds' A2DP node.

.DESCRIPTION
    Run from any PowerShell once test signing is on (docs/SETUP.md section 1). The build runs as you; the install
    part re-launches itself elevated (one UAC prompt). Steps:

      1. cargo make (driver package, test-signed) + cargo build -p crossc-cli
      2. preflight: test signing on, package complete, buds' A2DP node found (by the INF's hardware ID)
      3. trust the test certificate (LocalMachine Root + TrustedPublisher), only if not trusted yet
      4. remove previously staged crossc_bth packages, stage the new one and install it (pnputil /install)
      5. verify: node runs service crossc_bth with problem code 0; print the device stack

    -Uninstall reverses step 4 (the buds go back to BthA2dp); add -RemoveCert to also untrust the certificate.

.EXAMPLE
    .\tools\install-driver.ps1                 # debug build + install
.EXAMPLE
    .\tools\install-driver.ps1 -Release        # release build + install
.EXAMPLE
    .\tools\install-driver.ps1 -NoBuild        # reinstall the last build
.EXAMPLE
    .\tools\install-driver.ps1 -Uninstall -RemoveCert
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$NoBuild,
    [switch]$Uninstall,
    [switch]$RemoveCert,
    # Override the auto-detected BTHENUM instance id of the buds' A2DP node.
    [string]$InstanceId,
    # Internal: set when the script re-launches itself elevated, with the log file the parent prints.
    [switch]$Elevated,
    [string]$LogPath
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$Repo      = Split-Path -Parent $PSScriptRoot
$DriverDir = Join-Path $Repo 'drivers\crossc-bth'
$Flavor    = if ($Release) { 'release' } else { 'debug' }
$PkgDir    = Join-Path $DriverDir "target\$Flavor\crossc_bth_package"
$InfName   = 'crossc_bth.inf'
$Service   = 'crossc_bth'
$A2dpUuid  = '{0000110B-0000-1000-8000-00805F9B34FB}'

function Step([string]$m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Ok([string]$m)   { Write-Host "    $m" -ForegroundColor Green }
function Warn([string]$m) { Write-Host "    $m" -ForegroundColor Yellow }
function Fail([string]$m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal $id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# ---------------------------------------------------------------------------------------------------------------
# Build (as the calling user)
# ---------------------------------------------------------------------------------------------------------------
function Invoke-Build {
    if (-not $env:LIBCLANG_PATH) {
        $llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
        if (Test-Path (Join-Path $llvm 'libclang.dll')) { $env:LIBCLANG_PATH = $llvm }
        else { Fail "LIBCLANG_PATH is not set and $llvm\libclang.dll was not found (winget install LLVM.LLVM)" }
    }
    foreach ($tool in 'cargo', 'cargo-make') {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Fail "$tool not found on PATH" }
    }

    Step "Building driver package ($Flavor)"
    Push-Location $DriverDir
    try {
        if ($Release) { cargo make default --release } else { cargo make }
        if ($LASTEXITCODE -ne 0) { Fail "cargo make failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
    Ok $PkgDir

    Step 'Building crossc-cli (release)'
    Push-Location $Repo
    try {
        cargo build --release -p crossc-cli
        if ($LASTEXITCODE -ne 0) { Fail "cargo build -p crossc-cli failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
    Ok (Join-Path $Repo 'target\release\crossc-cli.exe')
}

# ---------------------------------------------------------------------------------------------------------------
# Elevated helpers
# ---------------------------------------------------------------------------------------------------------------
function Test-TestSigning {
    $out = (bcdedit /enum '{current}') -join "`n"
    if ($LASTEXITCODE -ne 0) { Fail "bcdedit failed: $out" }
    return [bool]($out -match '(?im)^\s*testsigning\s+Yes\s*$')
}

# The buds-specific hardware ID from the INF's model line (the INF carries no generic A2DP ID by design).
function Get-InfHardwareId([string]$inf) {
    $line = Select-String -Path $inf -Pattern 'BTHENUM\\\{0000110b-[^,\s]*_VID&[0-9a-f]+_PID&[0-9a-f]+' -AllMatches |
        Select-Object -First 1
    if (-not $line) { Fail "no BTHENUM ..._VID&..._PID&... model line in $inf" }
    return $line.Matches[0].Value
}

function Find-BudsNode([string]$hwid) {
    $nodes = Get-PnpDevice | Where-Object { $_.InstanceId -like "BTHENUM\$A2dpUuid*" }
    foreach ($n in $nodes) {
        $ids = (Get-PnpDeviceProperty -InstanceId $n.InstanceId -KeyName DEVPKEY_Device_HardwareIds).Data
        if ($ids -and ($ids | Where-Object { $_ -ieq $hwid })) { return $n }
    }
    return $null
}

function Get-NodeState([string]$id) {
    $p = Get-PnpDeviceProperty -InstanceId $id -KeyName DEVPKEY_Device_Service, DEVPKEY_Device_ProblemCode,
        DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_Stack, DEVPKEY_Device_DriverVersion
    $get = { param($k) ($p | Where-Object KeyName -eq $k).Data }
    [pscustomobject]@{
        Service = & $get 'DEVPKEY_Device_Service'
        Problem = & $get 'DEVPKEY_Device_ProblemCode'
        Inf     = & $get 'DEVPKEY_Device_DriverInfPath'
        Version = & $get 'DEVPKEY_Device_DriverVersion'
        Stack   = (& $get 'DEVPKEY_Device_Stack') -join ' -> '
    }
}

# Published names (oemNN.inf) of every staged package whose original name is crossc_bth.inf.
function Get-StagedPackages {
    $out = pnputil /enum-drivers
    $published = $null
    $result = @()
    foreach ($l in $out) {
        if ($l -match '^\s*Published Name\s*:\s*(\S+)') { $published = $Matches[1] }
        elseif ($l -match '^\s*Original Name\s*:\s*(\S+)' -and $published) {
            if ($Matches[1] -ieq $InfName) { $result += $published }
            $published = $null
        }
    }
    return $result
}

function Remove-StagedPackages {
    $pkgs = @(Get-StagedPackages)
    if ($pkgs.Count -eq 0) { Ok 'no crossc_bth package staged'; return }
    foreach ($p in $pkgs) {
        Step "Removing staged package $p"
        pnputil /delete-driver $p /uninstall /force | Out-Host
        if ($LASTEXITCODE -notin 0, 3010) { Warn "pnputil /delete-driver $p exited $LASTEXITCODE" }
    }
}

function Get-CertThumbprint([string]$cer) {
    (New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cer).Thumbprint
}

function Install-Cert([string]$cer) {
    $tp = Get-CertThumbprint $cer
    foreach ($store in 'Root', 'TrustedPublisher') {
        if (Test-Path "Cert:\LocalMachine\$store\$tp") { Ok "certificate already trusted in LocalMachine\$store"; continue }
        Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
        Ok "certificate $tp added to LocalMachine\$store"
    }
}

function Remove-Cert {
    foreach ($store in 'Root', 'TrustedPublisher') {
        $certs = @(Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object Subject -eq 'CN=WDRLocalTestCert')
        foreach ($c in $certs) { Remove-Item $c.PSPath; Ok "removed $($c.Thumbprint) from LocalMachine\$store" }
    }
    Warn 'the signing copy (with private key) stays in your user store WDRTestCertStore; see docs/SETUP.md 8.3'
}

function Show-State([string]$id, [string]$expect) {
    $s = Get-NodeState $id
    Write-Host ("    Service : {0}`n    Problem : {1}`n    INF     : {2} (version {3})`n    Stack   : {4}" -f `
        $s.Service, $s.Problem, $s.Inf, $s.Version, $s.Stack)
    if ($s.Service -ieq $expect -and ($s.Problem -eq 0 -or $null -eq $s.Problem)) { return $true }
    switch ($s.Problem) {
        52 { Warn 'problem 52: signature not trusted (test signing off, Secure Boot on, or cert not trusted)' }
        10 { Warn 'problem 10: start failed; see the driver trace in DebugView (docs/SETUP.md section 6)' }
        31 { Warn 'problem 31: driver failed to load; see C:\Windows\INF\setupapi.dev.log' }
        39 { Warn 'problem 39: driver failed to load (often HVCI / Memory integrity); see setupapi.dev.log' }
        default { if ($s.Problem) { Warn "problem code $($s.Problem)" } }
    }
    return $false
}

# ---------------------------------------------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------------------------------------------
if (-not $Elevated) {
    if (-not $Uninstall -and -not $NoBuild) { Invoke-Build }
    if (-not (Test-Admin)) {
        Step 'Re-launching elevated for the install (UAC prompt)'
        $shell = (Get-Process -Id $PID).Path
        $argv = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Elevated', '-NoBuild')
        if ($Release)    { $argv += '-Release' }
        if ($Uninstall)  { $argv += '-Uninstall' }
        if ($RemoveCert) { $argv += '-RemoveCert' }
        if ($InstanceId) { $argv += @('-InstanceId', "`"$InstanceId`"") }
        $log = Join-Path $env:TEMP 'crossc-install-driver.log'
        Remove-Item $log -ErrorAction SilentlyContinue
        $argv += @('-LogPath', "`"$log`"")
        try {
            $p = Start-Process -FilePath $shell -ArgumentList $argv -Verb RunAs -Wait -PassThru
        } catch { Fail "elevation was cancelled or failed: $($_.Exception.Message)" }
        if (Test-Path $log) { Get-Content $log | Out-Host }
        exit $p.ExitCode
    }
}

# From here on: elevated. When re-launched, mirror output to a log the non-elevated parent prints.
$transcript = $false
if ($Elevated -and $LogPath) {
    try { Start-Transcript -Path $LogPath -Force | Out-Null; $transcript = $true } catch { }
}
try {
    $inf = Join-Path $PkgDir $InfName

    # Locate the node (needs the INF for its hardware ID unless -InstanceId was given).
    $node = $null
    if ($InstanceId) {
        $node = Get-PnpDevice -InstanceId $InstanceId -ErrorAction SilentlyContinue
        if (-not $node) { Fail "no device with instance id $InstanceId" }
    } elseif (Test-Path $inf) {
        $hwid = Get-InfHardwareId $inf
        $node = Find-BudsNode $hwid
        if (-not $node) { Warn "no A2DP node with hardware id $hwid (are the buds paired, LE Audio off?)" }
    }

    if ($Uninstall) {
        Step 'Uninstalling crossc_bth'
        Remove-StagedPackages
        if ($RemoveCert) { Step 'Untrusting the test certificate'; Remove-Cert }
        if ($node) {
            pnputil /scan-devices | Out-Null
            Step "Node $($node.InstanceId)"
            if (Show-State $node.InstanceId 'BthA2dp') { Ok 'buds are back on BthA2dp' }
            else { Warn 'node is not on BthA2dp yet: Device Manager -> node -> Update driver -> pick "Microsoft Bluetooth A2dp Source", or re-pair the buds' }
        }
        exit 0
    }

    Step 'Preflight'
    if (-not (Test-TestSigning)) {
        Fail 'test signing is off. Run (admin): bcdedit /set testsigning on, then reboot. Secure Boot must be off (docs/SETUP.md section 1).'
    }
    Ok 'test signing is on'
    try { if (Confirm-SecureBootUEFI) { Warn 'Secure Boot reports ON: test-signed drivers will not load until it is off' } } catch { }
    foreach ($f in 'crossc_bth.inf', 'crossc_bth.sys', 'crossc_bth.cat', 'WDRLocalTestCert.cer') {
        if (-not (Test-Path (Join-Path $PkgDir $f))) { Fail "$f missing in $PkgDir (build first, or drop -NoBuild)" }
    }
    Ok "package: $PkgDir"
    if (-not $node) { Fail 'buds A2DP node not found; pair the buds (LE Audio off) or pass -InstanceId' }
    Ok "node: $($node.InstanceId)"

    Step 'Trusting the test certificate'
    Install-Cert (Join-Path $PkgDir 'WDRLocalTestCert.cer')

    Step 'Removing older crossc_bth packages'
    Remove-StagedPackages

    Step 'Staging and installing the new package'
    pnputil /add-driver $inf /install | Out-Host
    $rc = $LASTEXITCODE
    if ($rc -eq 3010) { Warn 'pnputil says a reboot is required to finish' }
    elseif ($rc -ne 0) { Warn "pnputil /add-driver exited $rc (checking the node anyway)" }

    Step 'Verifying'
    if (Show-State $node.InstanceId $Service) {
        Ok 'crossc_bth is running on the buds A2DP node'
        Write-Host ''
        Write-Host "Next (admin): `"$Repo\target\release\crossc-cli.exe`" probe   (trace: DebugView, docs/SETUP.md section 6)"
        exit 0
    }
    if ((Get-NodeState $node.InstanceId).Service -ine $Service) {
        Warn 'the node did not switch to crossc_bth. Ranking check:'
        pnputil /enum-devices /instanceid $node.InstanceId /drivers | Out-Host
    }
    exit 2
} finally {
    if ($transcript) { try { Stop-Transcript | Out-Null } catch { } }
}
