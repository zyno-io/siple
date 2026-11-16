<#
.SYNOPSIS
  Builds the Windows release and zips it: dist\Siple-<version>-windows-x64.zip

.DESCRIPTION
  Run on the Windows host from a normal PowerShell (Rust MSVC toolchain + Visual Studio with the
  C++ workload installed; Windows 10 SDK for rc.exe):

      powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1

  Steps: pick VS's bundled cmake (needed to build libopus statically; an old Strawberry cmake on
  PATH predates VS 2026) -> cargo build --release (CRT linked statically through
  .cargo\config.toml; the manifest + icon are embedded by crates\siple\build.rs) -> check
  `dumpbin /dependents` shows no VC runtime / opus DLLs -> zip siple.exe, siple-cli.exe,
  README and config example.

  NOTE: written without access to a Windows machine at the time; first run may need tweaks.

  Release signing (Azure Artifact Signing) happens between two runs of this script, because the
  zip must contain the signed exes:  -StageOnly  (build + stage, no zip)  ->  sign the exes in the
  stage directory  ->  -ZipOnly -RequireSigned  (verify Authenticode signatures, then zip).

.PARAMETER SkipBuild
  Reuse the existing target\release binaries.
.PARAMETER StageOnly
  Build and stage dist\Siple-<version>-windows-x64\ but do not zip it.
.PARAMETER ZipOnly
  Skip build and staging; zip the existing stage directory.
.PARAMETER RequireSigned
  Fail unless siple.exe and siple-cli.exe in the stage directory have a Valid, timestamped
  Authenticode signature (use with -ZipOnly for releases).
#>
param([switch]$SkipBuild, [switch]$StageOnly, [switch]$ZipOnly, [switch]$RequireSigned)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
$Target = 'x86_64-pc-windows-msvc'

# --- version -----------------------------------------------------------------------------------
$cargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw 'cannot read the version from Cargo.toml'
}
$Version = $Matches[1]
Write-Host "==> Siple $Version ($Target)"
$Dist = Join-Path $Root 'dist'
$Stage = Join-Path $Dist "Siple-$Version-windows-x64"
if ($StageOnly -and $ZipOnly) { throw '-StageOnly and -ZipOnly are mutually exclusive' }
if ($ZipOnly) { $SkipBuild = $true }

if (-not $ZipOnly) {
    # --- Visual Studio tools -----------------------------------------------------------------------
    $VsPath = $null
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $VsPath = & $vswhere -latest -products '*' -property installationPath | Select-Object -First 1
    }

    if (-not $env:CMAKE) {
        if ($VsPath) {
            $cm = Join-Path $VsPath 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
            if (Test-Path $cm) {
                $env:CMAKE = $cm
                Write-Host "==> CMAKE = $cm"
            }
        }
        if (-not $env:CMAKE) {
            Write-Warning 'VS-bundled cmake not found; using whatever cmake is on PATH (a stale one can break the libopus build)'
        }
    } else {
        Write-Host "==> CMAKE (from environment) = $env:CMAKE"
    }

    # --- build ---------------------------------------------------------------------------------------
    $Bin = Join-Path $Root "target\$Target\release"
    if (-not $SkipBuild) {
        if ($env:RUSTFLAGS) {
            Write-Warning 'RUSTFLAGS is set and overrides .cargo\config.toml: make sure it contains -C target-feature=+crt-static'
        }
        # libopus (built by CMake) must use the static runtime too; see the comment in this file.
        $toolchainVar = "CMAKE_TOOLCHAIN_FILE_$Target"
        if (-not [Environment]::GetEnvironmentVariable($toolchainVar)) {
            $tc = Join-Path $Root 'packaging\windows\static-crt.cmake'
            [Environment]::SetEnvironmentVariable($toolchainVar, $tc)
            Write-Host "==> $toolchainVar = $tc"
        }
        Write-Host '==> cargo build --release'
        # Explicit --target so the target-specific rustflags (crt-static) from .cargo\config.toml apply
        # and the output lands in target\<triple>\release.
        & cargo build --release --locked --target $Target -p siple -p siple-cli
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    $Gui = Join-Path $Bin 'siple.exe'
    $Cli = Join-Path $Bin 'siple-cli.exe'
    foreach ($f in @($Gui, $Cli)) { if (-not (Test-Path $f)) { throw "missing $f" } }

    # --- dependency check ------------------------------------------------------------------------------
    $dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
    if (-not $dumpbin -and $VsPath) {
        $dumpbin = Get-ChildItem (Join-Path $VsPath 'VC\Tools\MSVC') -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match 'Hostx64\\x64' } |
            Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $dumpbin) {
        Write-Warning 'dumpbin.exe not found (run from a "Developer PowerShell for VS"); skipping the dependency check'
    } else {
        # Anything here means the CRT / opus is not statically linked.
        $bad = '^(vcruntime|msvcp|vcomp|concrt|ucrtbase|api-ms-win-crt-|opus)'
        $failed = $false
        foreach ($exe in @($Gui, $Cli)) {
            $deps = & $dumpbin /dependents $exe |
                ForEach-Object { $_.Trim() } |
                Where-Object { $_ -match '\.dll$' }
            Write-Host ("==> {0} depends on: {1}" -f (Split-Path -Leaf $exe), ($deps -join ', '))
            $offenders = $deps | Where-Object { $_ -match $bad }
            if ($offenders) {
                Write-Error ("{0} depends on non-system DLLs: {1}" -f (Split-Path -Leaf $exe), ($offenders -join ', ')) -ErrorAction Continue
                $failed = $true
            }
        }
        if ($failed) { throw 'dependency check failed (is +crt-static active? see .cargo\config.toml)' }
        Write-Host '==> dependency check OK (system DLLs only)'
    }

    # --- stage + zip -------------------------------------------------------------------------------------
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    New-Item -ItemType Directory -Path $Stage -Force | Out-Null
    Copy-Item $Gui $Stage
    Copy-Item $Cli $Stage
    Copy-Item (Join-Path $Root 'README.md') $Stage
    Copy-Item (Join-Path $Root 'crates\siple-config\config.example.toml') $Stage
    Copy-Item (Join-Path $Root 'LICENSE') $Stage
    # Third-party notices: skipped with a warning in dev builds, fatal when SIPLE_RELEASE=1.
    $null = & cargo about --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        $notices = Join-Path $Stage 'THIRD-PARTY-NOTICES.html'
        & cargo about generate --locked -c (Join-Path $Root 'about.toml') (Join-Path $Root 'about.hbs') | Set-Content -Encoding utf8 $notices
        if ($LASTEXITCODE -ne 0) { throw "cargo about failed ($LASTEXITCODE)" }
    } elseif ($env:SIPLE_RELEASE -eq '1') {
        throw 'cargo-about is not installed (cargo install cargo-about --locked); required when SIPLE_RELEASE=1'
    } else {
        Write-Warning 'cargo-about not installed: skipping THIRD-PARTY-NOTICES.html (dev build)'
    }
    $docs = Join-Path $Stage 'docs'
    New-Item -ItemType Directory -Path $docs -Force | Out-Null
    foreach ($d in 'CONFIG.md', 'API.md') {
        $p = Join-Path $Root "docs\$d"
        if (Test-Path $p) { Copy-Item $p $docs }
    }
}

if ($StageOnly) {
    Write-Host "==> staged (not zipped): $Stage"
    return
}

# --- verify signatures (releases) -------------------------------------------------------------------
if ($RequireSigned) {
    foreach ($name in 'siple.exe', 'siple-cli.exe') {
        $f = Join-Path $Stage $name
        if (-not (Test-Path $f)) { throw "missing $f" }
        $sig = Get-AuthenticodeSignature -FilePath $f
        if ($sig.Status -ne 'Valid') { throw "Authenticode signature of $name is not Valid: $($sig.Status)" }
        if (-not $sig.TimeStamperCertificate) { throw "Authenticode signature of $name has no timestamp" }
        Write-Host "==> $name signed: $($sig.SignerCertificate.Subject)"
    }
}

$Zip = Join-Path $Dist "Siple-$Version-windows-x64.zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip
Write-Host "==> $Zip"
Get-FileHash $Zip -Algorithm SHA256 | ForEach-Object { Write-Host ("    sha256 " + $_.Hash) }
