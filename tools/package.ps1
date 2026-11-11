<#
.SYNOPSIS
    Assembles the crossc release bundle (zip, symbols zip, SHA256SUMS) from existing build outputs.

.DESCRIPTION
    Builds nothing. Run it after

      cd drivers\crossc-bth; cargo make default --release
      $env:RUSTFLAGS = '-C target-feature=+crt-static'             (see below)
      cargo build --release -p crossc-cli -p crossc-installer      (and -p crossc-service once that crate exists)

    Output, under -OutDir (default <RepoRoot>\dist):

      crossc-<version>-x64\            the bundle, unpacked
        crossc-installer.exe
        crossc-cli.exe
        crossc-service.exe             only if crates\crossc-service exists (then it must have been built)
        driver\                        crossc_bth.sys, crossc_bth.inf, crossc_bth.cat, WDRLocalTestCert.cer
        README.md, LICENSE             if present at the repo root
        TEST-SIGNING.md                what the test signature means for whoever installs this
        licenses\ssc-enc\              NOTICE, LICENSE-THIRD-PARTY
        THIRD-PARTY-LICENSES.html      cargo-about report for both Cargo workspaces
        BUILD-INFO.txt                 version, commit, toolchain, signing certificate fingerprints
        SHA256SUMS.txt                 every file of the bundle
      crossc-<version>-x64.zip         the same directory, zipped
      crossc-<version>-symbols.zip     the .pdb files (never part of the bundle)
      SHA256SUMS.txt                   the two zips

    Before copying, the script checks that crossc_bth.sys and crossc_bth.cat are signed by the certificate in
    WDRLocalTestCert.cer: a package whose .cer does not match its signature cannot be installed.

    It also reads the import table of each program. One that was built without +crt-static imports
    VCRUNTIME140.dll and does not start on a PC without the Visual C++ Redistributable, which the bundle does
    not carry. In CI that is an error; a local build gets a warning and a line in BUILD-INFO.txt.

    THIRD-PARTY-LICENSES.html needs cargo-about (https://github.com/EmbarkStudios/cargo-about) on PATH. Without
    it the script warns and leaves the file out, unless -RequireLicenses is given or the script runs in CI, where
    a missing tool is an error. Configuration: .github\release\about.toml (accepted licenses) and about.hbs.

.PARAMETER Version
    Bundle version, with or without a leading "v". Default: [workspace.package] version of the root Cargo.toml
    if it has one, otherwise dev-<short commit id>.

.PARAMETER OutDir
    Where the bundle goes. Default: <RepoRoot>\dist.

.PARAMETER RepoRoot
    Source tree that README.md, LICENSE, the ssc-enc notices and the Cargo manifests are read from.
    Default: the parent of this script's directory.

.PARAMETER BuildRoot
    Tree that holds the build outputs (target\<flavor>\*.exe and
    drivers\crossc-bth\target\<flavor>\crossc_bth_package). Default: RepoRoot. Only read, never written.

.PARAMETER Flavor
    Cargo profile directory of the build outputs: release (default) or debug.

.PARAMETER LicensesOnly
    Only write <OutDir>\THIRD-PARTY-LICENSES.html and exit. Needs no build outputs; CI runs it before the long
    builds so a dependency with a license that is not on the accepted list fails the job early.

.PARAMETER LicensesFile
    A report written earlier by -LicensesOnly to put into the bundle instead of running cargo-about again.
    CI uses it: the report takes minutes to generate and only changes when a dependency does.

.PARAMETER SkipLicenses
    Leave THIRD-PARTY-LICENSES.html out without looking for cargo-about.

.PARAMETER RequireLicenses
    Fail if cargo-about is missing (implied when the CI environment variable is set).

.EXAMPLE
    .\tools\package.ps1
.EXAMPLE
    .\tools\package.ps1 -Version 0.2.0 -OutDir C:\temp\crossc-dist
.EXAMPLE
    .\tools\package.ps1 -RepoRoot C:\src\crossc-worktree -BuildRoot C:\src\crossc    # package another tree's build
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$OutDir,
    [string]$RepoRoot,
    [string]$BuildRoot,
    [ValidateSet('release', 'debug')]
    [string]$Flavor = 'release',
    [switch]$LicensesOnly,
    [string]$LicensesFile,
    [switch]$SkipLicenses,
    [switch]$RequireLicenses
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

# Files that live next to this script's repo copy (not in -RepoRoot, so a -RepoRoot fixture needs none of them).
$ResourceDir = Join-Path (Split-Path -Parent $PSScriptRoot) '.github\release'
$Utf8NoBom = New-Object System.Text.UTF8Encoding $false

function Resolve-Dir([string]$Path, [string]$What) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { throw "$What not found: $Path" }
    (Resolve-Path -LiteralPath $Path).ProviderPath.TrimEnd('\')
}

function Write-TextFile([string]$Path, [string]$Text) {
    # UTF-8 without BOM and with LF line ends, the same from Windows PowerShell 5.1 and PowerShell 7.
    [System.IO.File]::WriteAllText($Path, ($Text -replace "`r`n", "`n"), $Utf8NoBom)
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Copy-Required([string]$Source, [string]$DestinationDir) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "Missing build output: $Source" }
    Copy-Item -LiteralPath $Source -Destination $DestinationDir
}

function Get-WorkspaceVersion([string]$CargoToml) {
    $inSection = $false
    foreach ($line in [System.IO.File]::ReadAllLines($CargoToml)) {
        if ($line -match '^\s*\[(.+?)\]\s*(#.*)?$') { $inSection = ($Matches[1].Trim() -eq 'workspace.package'); continue }
        if ($inSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    }
    $null
}

function Get-GitValue([string[]]$GitArgs) {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { return $null }
    $out = $null
    try { $out = & git -C $RepoRoot @GitArgs 2>$null } catch { return $null }
    if ($LASTEXITCODE -ne 0) { return $null }
    if ($null -eq $out) { return '' }
    (@($out) -join "`n").Trim()
}

function Get-Certificate([string]$Path) {
    New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList $Path
}

function Get-CertSha256([System.Security.Cryptography.X509Certificates.X509Certificate2]$Cert) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { ([System.BitConverter]::ToString($sha.ComputeHash($Cert.RawData)) -replace '-', '') } finally { $sha.Dispose() }
}

function Assert-SignedBy([string]$File, [System.Security.Cryptography.X509Certificates.X509Certificate2]$Cert) {
    $sig = Get-AuthenticodeSignature -LiteralPath $File
    if ($null -eq $sig.SignerCertificate) {
        throw "$File carries no Authenticode signature (status: $($sig.Status))."
    }
    if ($sig.SignerCertificate.Thumbprint -ne $Cert.Thumbprint) {
        throw ("$File is signed by $($sig.SignerCertificate.Subject) [$($sig.SignerCertificate.Thumbprint)], " +
            "but WDRLocalTestCert.cer is [$($Cert.Thumbprint)]. The package is inconsistent: rebuild the driver.")
    }
}

# Names of the DLLs a PE file imports: the import table and the delay-load import table.
function Get-PeImports([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $pe = 0
    if ($bytes.Length -gt 0x40) { $pe = [System.BitConverter]::ToInt32($bytes, 0x3C) }
    if ($pe -le 0 -or $pe + 24 -gt $bytes.Length -or [System.BitConverter]::ToUInt32($bytes, $pe) -ne 0x4550) {
        throw "$Path is not a PE file."
    }
    $optional = $pe + 24
    switch ([System.BitConverter]::ToUInt16($bytes, $optional)) {
        0x20B { $directories = $optional + 112 }    # PE32+
        0x10B { $directories = $optional + 96 }     # PE32
        default { throw "$Path has an unknown optional header." }
    }
    $sectionTable = $optional + [System.BitConverter]::ToUInt16($bytes, $pe + 20)
    $sections = foreach ($i in 0..([System.BitConverter]::ToUInt16($bytes, $pe + 6) - 1)) {
        $header = $sectionTable + 40 * $i
        [pscustomobject]@{
            Rva  = [System.BitConverter]::ToUInt32($bytes, $header + 12)
            Size = [Math]::Max([System.BitConverter]::ToUInt32($bytes, $header + 8),
                [System.BitConverter]::ToUInt32($bytes, $header + 16))
            File = [System.BitConverter]::ToUInt32($bytes, $header + 20)
        }
    }
    $toOffset = {
        param([uint32]$Rva)
        foreach ($section in $sections) {
            if ($Rva -ge $section.Rva -and $Rva -lt $section.Rva + $section.Size) {
                return [int]($Rva - $section.Rva + $section.File)
            }
        }
        throw "$Path has an import table that points outside its sections."
    }

    $names = New-Object System.Collections.Generic.List[string]
    # Per table: index in the data directory, size of one descriptor, where in it the RVA of the DLL name is.
    foreach ($table in @(@(1, 20, 12), @(13, 32, 4))) {
        $tableRva = [System.BitConverter]::ToUInt32($bytes, $directories + 8 * $table[0])
        if ($tableRva -eq 0) { continue }
        $descriptor = & $toOffset $tableRva
        while ($true) {
            $nameRva = [System.BitConverter]::ToUInt32($bytes, $descriptor + $table[2])
            if ($nameRva -eq 0) { break }    # the table ends with an all-zero descriptor
            $start = & $toOffset $nameRva
            $end = [System.Array]::IndexOf($bytes, [byte]0, $start)
            $names.Add([System.Text.Encoding]::ASCII.GetString($bytes, $start, $end - $start))
            $descriptor += $table[1]
        }
    }
    $names.ToArray()
}

function New-Zip([string]$SourceDir, [string]$ZipPath) {
    # Entry by entry rather than ZipFile.CreateFromDirectory: on Windows PowerShell 5.1 (.NET Framework) that call
    # writes "\" as the path separator, which other unzip tools do not treat as a directory separator.
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    # The archive unpacks into one top-level folder named like the source directory.
    $base = Split-Path -Leaf $SourceDir
    $files = @(Get-ChildItem -LiteralPath $SourceDir -Recurse -File | Sort-Object FullName)
    $archive = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in $files) {
            $entry = $base + '/' + ($file.FullName.Substring($SourceDir.Length + 1) -replace '\\', '/')
            [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive, $file.FullName, $entry, [System.IO.Compression.CompressionLevel]::Optimal)
        }
    }
    finally { $archive.Dispose() }
}

function Write-Sha256Sums([string]$Dir, [string[]]$RelativePaths, [string]$OutFile) {
    $lines = foreach ($rel in ($RelativePaths | Sort-Object)) {
        '{0}  {1}' -f (Get-Sha256 (Join-Path $Dir $rel)), ($rel -replace '\\', '/')
    }
    Write-TextFile $OutFile ((@($lines) -join "`n") + "`n")
}

# One cargo-about run per Cargo workspace; returns the HTML fragment for that workspace.
function Get-LicenseFragment([string]$Manifest, [string]$Section, [string[]]$ExtraArgs) {
    $tmp = [System.IO.Path]::GetTempFileName()
    try {
        $cargoArgs = @('about', 'generate', '--locked', '--fail',
            '--config', (Join-Path $ResourceDir 'about.toml'),
            '--manifest-path', $Manifest) + $ExtraArgs + @(
            '--output-file', $tmp, (Join-Path $ResourceDir 'about.hbs'))
        Write-Host "cargo $($cargoArgs -join ' ')"
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw ("cargo about failed for $Manifest (exit code $LASTEXITCODE). If a dependency uses a license " +
                "that is not on the accepted list, review it and add it to .github\release\about.toml.")
        }
        # The template marks its anchors with @@SECTION@@ so the two fragments do not share ids.
        [System.IO.File]::ReadAllText($tmp).Replace('@@SECTION@@', $Section)
    }
    finally { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
}

# Writes THIRD-PARTY-LICENSES.html; returns $false if it was skipped.
function Write-ThirdPartyLicenses([string]$OutFile) {
    if ($SkipLicenses) {
        Write-Warning 'THIRD-PARTY-LICENSES.html left out (-SkipLicenses).'
        return $false
    }
    if (-not (Get-Command cargo-about -ErrorAction SilentlyContinue)) {
        if ($RequireLicenses -or $env:CI) {
            throw 'cargo-about is not on PATH; it is required here (CI or -RequireLicenses).'
        }
        Write-Warning ('cargo-about is not on PATH: THIRD-PARTY-LICENSES.html left out. ' +
            'Install it (cargo install --locked cargo-about) before building a bundle you hand to someone else.')
        return $false
    }

    $user = Get-LicenseFragment (Join-Path $RepoRoot 'Cargo.toml') 'user' @('--workspace')
    $driver = Get-LicenseFragment (Join-Path $RepoRoot 'drivers\crossc-bth\Cargo.toml') 'driver' @()

    # No version in here: the report is a function of the dependency graphs alone, which is what lets CI reuse it.
    $title = 'crossc: third-party licenses'
    $html = @"
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>$title</title>
<style>
body { font-family: system-ui, "Segoe UI", sans-serif; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; line-height: 1.45; }
h1, h2, h3 { line-height: 1.2; }
ul.licenses-list { list-style: none; padding: 0; }
li.license { border-top: 1px solid #8888; margin-top: 1.5rem; }
pre.license-text { white-space: pre-wrap; background: #8881; padding: 1rem; overflow-x: auto; }
</style>
</head>
<body>
<h1>$title</h1>
<p>crossc links the Rust crates listed below. This page was generated by cargo-about from the two Cargo.lock files
of the build (dependency graph for x86_64-pc-windows-msvc, without build-only and test-only crates). crossc's own
license is in <code>LICENSE</code>; the notices for the SSC encoder port are in <code>licenses\ssc-enc</code>.</p>
<ul>
<li><a href="#user">User-mode programs</a> (crossc-cli, crossc-installer and, if shipped, crossc-service)</li>
<li><a href="#driver">Kernel driver</a> (crossc_bth.sys)</li>
</ul>
<h1 id="user">User-mode programs</h1>
$user
<h1 id="driver">Kernel driver</h1>
$driver
</body>
</html>
"@
    Write-TextFile $OutFile $html
    $true
}

# ---------------------------------------------------------------------------------------------------------------

if (-not $RepoRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
$RepoRoot = Resolve-Dir $RepoRoot 'RepoRoot'
if (-not $BuildRoot) { $BuildRoot = $RepoRoot }
$BuildRoot = Resolve-Dir $BuildRoot 'BuildRoot'
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'Cargo.toml') -PathType Leaf)) {
    throw "RepoRoot has no Cargo.toml: $RepoRoot"
}

if (-not $Version) { $Version = Get-WorkspaceVersion (Join-Path $RepoRoot 'Cargo.toml') }
if (-not $Version) {
    $short = Get-GitValue @('rev-parse', '--short=7', 'HEAD')
    if (-not $short) { $short = 'unknown' }
    $Version = "dev-$short"
}
if ($Version -match '^v\d') { $Version = $Version.Substring(1) }
if ($Version -notmatch '^[0-9A-Za-z][0-9A-Za-z.+_-]*$') {
    throw "Version '$Version' is not usable in a file name (letters, digits, '.', '+', '_', '-' only)."
}

if (-not $OutDir) { $OutDir = Join-Path $RepoRoot 'dist' }
if (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path (Get-Location).ProviderPath $OutDir }
if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = Resolve-Dir $OutDir 'OutDir'
# Keeps the output out of `git status` when OutDir is inside a work tree, without touching the repo's .gitignore.
$outIgnore = Join-Path $OutDir '.gitignore'
if (-not (Test-Path -LiteralPath $outIgnore)) { Write-TextFile $outIgnore "*`n" }

if ($LicensesOnly) {
    $file = Join-Path $OutDir 'THIRD-PARTY-LICENSES.html'
    if (Write-ThirdPartyLicenses $file) { Write-Host "Wrote $file" }
    return
}

$bundleName = "crossc-$Version-x64"
$symbolsName = "crossc-$Version-symbols"
$bundleDir = Join-Path $OutDir $bundleName
$symbolsDir = Join-Path $OutDir $symbolsName
$bundleZip = "$bundleDir.zip"
$symbolsZip = "$symbolsDir.zip"
$sumsFile = Join-Path $OutDir 'SHA256SUMS.txt'

$binDir = Join-Path $BuildRoot "target\$Flavor"
$driverPkg = Join-Path $BuildRoot "drivers\crossc-bth\target\$Flavor\crossc_bth_package"
if (-not (Test-Path -LiteralPath $driverPkg -PathType Container)) {
    throw "Driver package not found: $driverPkg. Build it first (cargo make default --release in drivers\crossc-bth)."
}

Write-Host "Packaging crossc $Version"
Write-Host "  sources : $RepoRoot"
Write-Host "  builds  : $BuildRoot ($Flavor)"
Write-Host "  output  : $OutDir"

foreach ($stale in @($bundleDir, $symbolsDir)) {
    if (Test-Path -LiteralPath $stale) { Remove-Item -LiteralPath $stale -Recurse -Force }
}
foreach ($stale in @($bundleZip, $symbolsZip, $sumsFile)) {
    if (Test-Path -LiteralPath $stale) { Remove-Item -LiteralPath $stale -Force }
}
New-Item -ItemType Directory -Path $bundleDir, (Join-Path $bundleDir 'driver'), $symbolsDir | Out-Null

# --- driver package: the signature must belong to the certificate that ships next to it -------------------------
$cerPath = Join-Path $driverPkg 'WDRLocalTestCert.cer'
if (-not (Test-Path -LiteralPath $cerPath -PathType Leaf)) { throw "Missing build output: $cerPath" }
$cert = Get-Certificate $cerPath
foreach ($signed in 'crossc_bth.sys', 'crossc_bth.cat') {
    $path = Join-Path $driverPkg $signed
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing build output: $path" }
    Assert-SignedBy $path $cert
}
foreach ($name in 'crossc_bth.sys', 'crossc_bth.inf', 'crossc_bth.cat', 'WDRLocalTestCert.cer') {
    Copy-Required (Join-Path $driverPkg $name) (Join-Path $bundleDir 'driver')
}

# --- user-mode programs ----------------------------------------------------------------------------------------
$programs = @('crossc-installer', 'crossc-cli')
$hasService = Test-Path -LiteralPath (Join-Path $RepoRoot 'crates\crossc-service\Cargo.toml') -PathType Leaf
if ($hasService) {
    $programs += 'crossc-service'
}
elseif (Test-Path -LiteralPath (Join-Path $binDir 'crossc-service.exe') -PathType Leaf) {
    # Built from a tree that has the crate even though -RepoRoot does not: ship it rather than drop it silently.
    $programs += 'crossc-service'
}
foreach ($program in $programs) {
    Copy-Required (Join-Path $binDir "$program.exe") $bundleDir
}

# The bundle carries no C runtime DLLs: a program that imports them only starts where the Visual C++
# Redistributable happens to be installed. The release build links the runtime statically (+crt-static).
$isCi = ($env:GITHUB_ACTIONS -eq 'true')
$crtDll = '^(vcruntime|msvcp|msvcr|vccorlib|concrt|vcomp|ucrtbase|api-ms-win-crt-)'
$runtime = 'C runtime linked statically, no Visual C++ Redistributable needed'
$needCrt = foreach ($program in $programs) {
    $dlls = @(Get-PeImports (Join-Path $bundleDir "$program.exe") | Where-Object { $_ -match $crtDll } |
            ForEach-Object { $_ -replace '^api-ms-win-crt-.*', 'api-ms-win-crt-*' } | Sort-Object -Unique)
    if ($dlls.Count -ne 0) { "$program.exe imports $($dlls -join ', ')" }
}
if ($needCrt) {
    $message = ("$(@($needCrt) -join '; '). The bundle does not carry these DLLs. Build the programs with " +
        "RUSTFLAGS='-C target-feature=+crt-static'.")
    if ($isCi) { throw $message }
    Write-Warning ($message + ' As it is, this bundle only runs where the Visual C++ Redistributable is installed.')
    $runtime = 'C runtime linked dynamically: the Visual C++ Redistributable (x64) must be installed'
}

# --- symbols (separate zip) ------------------------------------------------------------------------------------
Copy-Required (Join-Path $driverPkg 'crossc_bth.pdb') $symbolsDir
foreach ($program in $programs) {
    $pdb = Join-Path $binDir (($program -replace '-', '_') + '.pdb')
    if (Test-Path -LiteralPath $pdb -PathType Leaf) { Copy-Item -LiteralPath $pdb -Destination $symbolsDir }
    else { Write-Warning "No symbols for $program.exe ($pdb not found)." }
}

# --- documents -------------------------------------------------------------------------------------------------
foreach ($doc in 'README.md', 'LICENSE') {
    $src = Join-Path $RepoRoot $doc
    if (Test-Path -LiteralPath $src -PathType Leaf) { Copy-Item -LiteralPath $src -Destination $bundleDir }
    else { Write-Warning "$doc not found at the repo root: the bundle ships without it." }
}

$sscLicenses = Join-Path $bundleDir 'licenses\ssc-enc'
New-Item -ItemType Directory -Path $sscLicenses -Force | Out-Null
foreach ($notice in 'NOTICE', 'LICENSE-THIRD-PARTY') {
    # Required: BSD-3-Clause asks for these notices in binary redistributions of the encoder.
    $src = Join-Path $RepoRoot "crates\ssc-enc\$notice"
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) { throw "Missing license notice: $src" }
    Copy-Item -LiteralPath $src -Destination $sscLicenses
}

$signingNote = [System.IO.File]::ReadAllText((Join-Path $ResourceDir 'TEST-SIGNING.md'))
if (-not $isCi) {
    $signingNote = @"
> **Local build.** This bundle was not produced by the release workflow. The private key of its signing
> certificate is still in the certificate store of the machine that built it (CurrentUser\WDRTestCertStore), so
> the statement below that the key no longer exists does not hold for this bundle.
"@ + "`n`n" + $signingNote
}
Write-TextFile (Join-Path $bundleDir 'TEST-SIGNING.md') $signingNote

if ($LicensesFile) {
    if (-not (Test-Path -LiteralPath $LicensesFile -PathType Leaf)) { throw "LicensesFile not found: $LicensesFile" }
    Copy-Item -LiteralPath $LicensesFile -Destination (Join-Path $bundleDir 'THIRD-PARTY-LICENSES.html')
}
else {
    [void](Write-ThirdPartyLicenses (Join-Path $bundleDir 'THIRD-PARTY-LICENSES.html'))
}

# --- BUILD-INFO.txt --------------------------------------------------------------------------------------------
$commit = Get-GitValue @('rev-parse', 'HEAD')
if (-not $commit) { $commit = 'unknown' }
elseif ($RepoRoot -ne $BuildRoot) { $commit += ' (source tree; the binaries come from another tree)' }
elseif (Get-GitValue @('status', '--porcelain', '--untracked-files=no')) { $commit += ' (with uncommitted changes)' }
if ($isCi) {
    $builder = "GitHub Actions, $($env:GITHUB_SERVER_URL)/$($env:GITHUB_REPOSITORY)/actions/runs/$($env:GITHUB_RUN_ID)"
}
else {
    $builder = 'local build'
}
$rustc = 'unknown'
if (Get-Command rustc -ErrorAction SilentlyContinue) { $rustc = (& rustc --version | Select-Object -First 1) }
$buildInfo = @"
crossc $Version (x64)

commit    : $commit
built     : $([DateTime]::UtcNow.ToString('yyyy-MM-dd HH:mm')) UTC
builder   : $builder
toolchain : $rustc
runtime   : $runtime
contents  : $($programs -join '.exe, ').exe, driver\crossc_bth.sys

Driver signing certificate (driver\WDRLocalTestCert.cer), self-signed test certificate:
  subject        : $($cert.Subject)
  valid          : $($cert.NotBefore.ToUniversalTime().ToString('yyyy-MM-dd')) to $($cert.NotAfter.ToUniversalTime().ToString('yyyy-MM-dd'))
  SHA-1 (thumb)  : $($cert.Thumbprint)
  SHA-256        : $(Get-CertSha256 $cert)

See TEST-SIGNING.md before installing the driver.
"@
Write-TextFile (Join-Path $bundleDir 'BUILD-INFO.txt') ($buildInfo + "`n")

# --- checksums and archives ------------------------------------------------------------------------------------
$bundleFiles = @(Get-ChildItem -LiteralPath $bundleDir -Recurse -File |
        ForEach-Object { $_.FullName.Substring($bundleDir.Length + 1) })
Write-Sha256Sums $bundleDir $bundleFiles (Join-Path $bundleDir 'SHA256SUMS.txt')

New-Zip $bundleDir $bundleZip
New-Zip $symbolsDir $symbolsZip
Remove-Item -LiteralPath $symbolsDir -Recurse -Force
Write-Sha256Sums $OutDir @("$bundleName.zip", "$symbolsName.zip") $sumsFile

Write-Host ''
Write-Host "Bundle  : $bundleZip"
Write-Host "Symbols : $symbolsZip"
Write-Host "Sums    : $sumsFile"
Write-Host ''
Get-ChildItem -LiteralPath $bundleDir -Recurse -File | ForEach-Object {
    Write-Host ('  {0,12:N0}  {1}' -f $_.Length, $_.FullName.Substring($bundleDir.Length + 1))
}

if ($env:GITHUB_OUTPUT) {
    $outputs = @(
        "version=$Version",
        "bundle-name=$bundleName",
        "bundle-zip=$bundleZip",
        "symbols-zip=$symbolsZip",
        "sha256sums=$sumsFile"
    )
    [System.IO.File]::AppendAllText($env:GITHUB_OUTPUT, (($outputs -join "`n") + "`n"), $Utf8NoBom)
}
