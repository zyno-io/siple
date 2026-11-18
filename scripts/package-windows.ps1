<#
.SYNOPSIS
  Builds the Windows release and zips it: dist\Siple-<version>-windows-<arch>.zip  (arch: x64, arm64)

.DESCRIPTION
  Run on the Windows host from a normal PowerShell (Rust MSVC toolchain + Visual Studio with the
  C++ workload installed; Windows 10 SDK for rc.exe):

      powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1

  Steps (both architectures): pick VS's bundled cmake (needed to build libopus statically; an old Strawberry cmake on
  PATH predates VS 2026) -> cargo build --release (CRT linked statically through
  .cargo\config.toml; the manifest + icon are embedded by crates\siple\build.rs) -> check
  `dumpbin /dependents` shows no VC runtime / opus DLLs -> zip siple.exe, siple-cli.exe,
  README and config example.

  -Arch arm64 cross-compiles for Windows on ARM (aarch64-pc-windows-msvc) from an x64 or ARM64
  host. It needs the rustup target (rustup target add aarch64-pc-windows-msvc), the MSVC ARM64
  build tools (VS component "MSVC ... ARM64/ARM64EC build tools") and clang (`ring` compiles its
  ARM64 assembly with clang: LLVM on PATH, or VS's "C++ Clang tools" component, found below).

  Release signing (Azure Artifact Signing) happens between two runs of this script, because the
  zip must contain the signed exes:  -StageOnly  (build + stage, no zip)  ->  sign the exes in the
  stage directory  ->  -ZipOnly -RequireSigned  (verify Authenticode signatures, then zip).

.PARAMETER Store
  Microsoft Store / MSIX build: passes `--features store` (no update check) and names the stage
  directory (and zip) `...-windows-<arch>-store`. scripts\package-msix.ps1 consumes that stage.
.PARAMETER Arch
  x64 (default) or arm64.
.PARAMETER SkipBuild
  Reuse the existing target\release binaries.
.PARAMETER StageOnly
  Build and stage dist\Siple-<version>-windows-<arch>\ but do not zip it.
.PARAMETER ZipOnly
  Skip build and staging; zip the existing stage directory.
.PARAMETER RequireSigned
  Fail unless siple.exe and siple-cli.exe in the stage directory have a Valid, timestamped
  Authenticode signature (use with -ZipOnly for releases).
#>
param(
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',
    [switch]$SkipBuild, [switch]$StageOnly, [switch]$ZipOnly, [switch]$RequireSigned, [switch]$Store
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
$Target = @{ x64 = 'x86_64-pc-windows-msvc'; arm64 = 'aarch64-pc-windows-msvc' }[$Arch]

# --- version -----------------------------------------------------------------------------------
$cargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') {
    throw 'cannot read the version from Cargo.toml'
}
$Version = $Matches[1]
Write-Host "==> Siple $Version ($Target)"
$Dist = Join-Path $Root 'dist'
$Suffix = if ($Store) { '-store' } else { '' }
$Stage = Join-Path $Dist "Siple-$Version-windows-$Arch$Suffix"
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

    # --- clang (arm64 only) -------------------------------------------------------------------------
    # `ring` builds its aarch64-pc-windows-msvc assembly with clang. Use one on PATH, else LLVM's
    # default install dir, else the clang that ships with VS ("C++ Clang tools for Windows").
    if ($Arch -eq 'arm64' -and -not $SkipBuild -and -not (Get-Command clang.exe -ErrorAction SilentlyContinue)) {
        $llvmDirs = @((Join-Path $env:ProgramFiles 'LLVM\bin'))
        if ($VsPath) { $llvmDirs += (Join-Path $VsPath 'VC\Tools\Llvm\x64\bin'), (Join-Path $VsPath 'VC\Tools\Llvm\ARM64\bin') }
        $llvm = $llvmDirs | Where-Object { Test-Path (Join-Path $_ 'clang.exe') } | Select-Object -First 1
        if (-not $llvm) { throw 'clang.exe not found (needed by ring for arm64): install LLVM or the VS "C++ Clang tools" component' }
        $env:PATH = "$llvm;$env:PATH"
        Write-Host "==> clang from $llvm"
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
        $features = @()
        if ($Store) { $features = @('--features', 'store') }
        Write-Host "==> cargo build --release $($features -join ' ')"
        # Explicit --target so the target-specific rustflags (crt-static) from .cargo\config.toml apply
        # and the output lands in target\<triple>\release.
        & cargo build --release --locked --target $Target -p siple -p siple-cli @features
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    $Gui = Join-Path $Bin 'siple.exe'
    $Cli = Join-Path $Bin 'siple-cli.exe'
    foreach ($f in @($Gui, $Cli)) { if (-not (Test-Path $f)) { throw "missing $f" } }

    # --- dependency check ------------------------------------------------------------------------------
    $dumpbin = (Get-Command dumpbin.exe -ErrorAction SilentlyContinue).Source
    if (-not $dumpbin -and $VsPath) {
        $dumpbin = Get-ChildItem (Join-Path $VsPath 'VC\Tools\MSVC') -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match 'Host(x64\\x64|arm64\\arm64)\\' } |
            Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $dumpbin) {
        Write-Warning 'dumpbin.exe not found (run from a "Developer PowerShell for VS"); skipping the dependency check'
    } else {
        # Anything here means the CRT / opus is not statically linked.
        $bad = '^(vcruntime|msvcp|vcomp|concrt|ucrtbase|api-ms-win-crt-|opus)'
        $failed = $false
        $machine = @{ x64 = '8664 machine (x64)'; arm64 = 'AA64 machine (ARM64)' }[$Arch]
        foreach ($exe in @($Gui, $Cli)) {
            $headers = (& $dumpbin /headers $exe) -join "`n"
            if ($headers -notmatch [regex]::Escape($machine)) {
                Write-Error ("{0} is not a {1} binary (expected '{2}')" -f (Split-Path -Leaf $exe), $Arch, $machine) -ErrorAction Continue
                $failed = $true
            }
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
        if ($failed) { throw 'binary check failed (wrong architecture, or +crt-static not active: see .cargo\config.toml)' }
        Write-Host "==> binary check OK ($Arch, system DLLs only)"
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
    # try/catch: with $ErrorActionPreference = Stop, a missing cargo-about surfaces as a terminating
    # error (cargo's "no such command" stderr under 5.1), not just a non-zero exit code.
    $haveAbout = $false
    try {
        $null = & cargo about --version 2>&1
        $haveAbout = ($LASTEXITCODE -eq 0)
    } catch {
        $haveAbout = $false
    }
    if ($haveAbout) {
        $notices = Join-Path $Stage 'THIRD-PARTY-NOTICES.html'
        # -o, not a pipe: cargo-about refuses PowerShell redirection (encoding issues).
        & cargo about generate --locked -c (Join-Path $Root 'about.toml') -o $notices (Join-Path $Root 'about.hbs')
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

$Zip = Join-Path $Dist "Siple-$Version-windows-$Arch$Suffix.zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
# Not Compress-Archive: under Windows PowerShell 5.1 it writes `\` into entry names (`docs\API.md`),
# which the zip spec forbids and unzip on macOS/Linux flattens. Name every entry with `/` ourselves.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$stageFull = (Resolve-Path $Stage).Path.TrimEnd('\') + '\'
$stream = [System.IO.File]::Open($Zip, [System.IO.FileMode]::CreateNew)
try {
    $archive = New-Object System.IO.Compression.ZipArchive($stream, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($f in Get-ChildItem -LiteralPath $Stage -Recurse -File | Sort-Object FullName) {
            $entry = $f.FullName.Substring($stageFull.Length).Replace('\', '/')
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive, $f.FullName, $entry, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally { $archive.Dispose() }
} finally { $stream.Dispose() }
# Check the result: portable separators, and every staged file present.
$check = [System.IO.Compression.ZipFile]::OpenRead($Zip)
try {
    $names = @($check.Entries | ForEach-Object { $_.FullName })
} finally { $check.Dispose() }
$bad = @($names | Where-Object { $_.Contains('\') })
if ($bad.Count) { throw "zip entries with backslashes: $($bad -join ', ')" }
$want = @(Get-ChildItem -LiteralPath $Stage -Recurse -File).Count
if ($names.Count -ne $want) { throw "zip has $($names.Count) entries, staged $want files" }
Write-Host "==> $Zip"
Get-FileHash $Zip -Algorithm SHA256 | ForEach-Object { Write-Host ("    sha256 " + $_.Hash) }
