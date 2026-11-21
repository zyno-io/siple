<#
.SYNOPSIS
  Builds the Microsoft Store package: one .msix per architecture and a bundle
  dist\msix\Siple_<version>_x64_arm64.msixbundle (unsigned: Partner Center signs it).

.DESCRIPTION
  Input: the stages made by `scripts\package-windows.ps1 -Arch <a> -StageOnly -Store`
  (dist\Siple-<cargo version>-windows-<a>-store\). Steps: render packaging\windows\AppxManifest.xml.in,
  lay out siple.exe, siple-cli.exe, LICENSE, THIRD-PARTY-NOTICES.html and Assets\, makepri (resources.pri),
  `makeappx pack` per architecture (semantic validation on), `makeappx bundle`, then validate the bundle
  (inner identities, versions, PE machine types, resources.pri).

  -TestSign additionally writes dist\msix\sideload\: a COPY of the bundle signed with a throwaway
  self-signed certificate (subject = Publisher, in LocalMachine\My: needs an elevated shell; the key is
  deleted afterwards), the exported .cer, install-test.ps1 and README.txt. The Store bundle itself stays
  unsigned.

  Needs the Windows 10/11 SDK (makeappx.exe, makepri.exe, signtool.exe), found under
  "%ProgramFiles(x86)%\Windows Kits\10\bin\<newest>\x64" or on PATH.

.PARAMETER MsixVersion
  Package version X.Y.Z.W (see scripts/store-version.sh). Default: <Cargo version>.0 (Store-eligible form).
.PARAMETER IdentityName / Publisher / PublisherDisplayName / DisplayName
  Must match Partner Center "Product identity" exactly for a Store upload. Defaults are test placeholders.
.PARAMETER Archs
  Architectures to package (default x64, arm64). Each needs its stage directory.
.PARAMETER StageVersion
  The Cargo version in the stage directory name. Default: read from Cargo.toml.
.PARAMETER TestSign
  Also produce the test-signed sideload copy (see above).
#>
param(
    [string]$MsixVersion,
    [string]$IdentityName = 'io.zyno.siple',
    [string]$Publisher = 'CN=Siple Test',
    [string]$PublisherDisplayName = 'Siple',
    [string]$DisplayName = 'Siple',
    [ValidateSet('x64', 'arm64')][string[]]$Archs = @('x64', 'arm64'),
    [string]$StageVersion,
    [switch]$TestSign
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

if (-not $StageVersion) {
    $cargoToml = Get-Content (Join-Path $Root 'Cargo.toml') -Raw
    if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') { throw 'cannot read the version from Cargo.toml' }
    $StageVersion = $Matches[1]
}
if (-not $MsixVersion) { $MsixVersion = "$StageVersion.0" }
if ($MsixVersion -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "MsixVersion '$MsixVersion' is not X.Y.Z.W" }
$Archs = @($Archs | Select-Object -Unique)
Write-Host "==> MSIX $MsixVersion ($($Archs -join ', ')) identity $IdentityName / $Publisher"

# --- SDK tools ----------------------------------------------------------------------------------
function Find-SdkTool([string]$name) {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $hit = $null
    if (Test-Path $kits) {
        $hit = Get-ChildItem $kits -Directory | Where-Object { $_.Name -match '^10\.' } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\$name" } |
            Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $hit) { $hit = (Get-Command $name -ErrorAction SilentlyContinue).Source }
    if (-not $hit) { throw "$name not found: install the Windows SDK (or put it on PATH)" }
    return $hit
}
$MakeAppx = Find-SdkTool 'makeappx.exe'
$MakePri = Find-SdkTool 'makepri.exe'
Write-Host "==> makeappx: $MakeAppx"

function Invoke-Tool([string]$exe, [string[]]$arguments) {
    # Native stderr must not become a terminating error under 5.1; judge by the exit code.
    $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    try { $out = & $exe @arguments 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) {
        $out | ForEach-Object { Write-Host $_ }
        throw "$(Split-Path -Leaf $exe) failed ($LASTEXITCODE)"
    }
    return $out
}

function Get-PeMachine([string]$path) {
    $b = [System.IO.File]::ReadAllBytes($path)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    return ('{0:X4}' -f [BitConverter]::ToUInt16($b, $pe + 4))
}
$PeMachine = @{ x64 = '8664'; arm64 = 'AA64' }

# --- work dirs ------------------------------------------------------------------------------------
$Out = Join-Path $Root 'dist\msix'
$Work = Join-Path $Root 'dist\msix-work'
foreach ($d in $Out, $Work) { if (Test-Path $d) { Remove-Item $d -Recurse -Force } ; New-Item -ItemType Directory -Path $d | Out-Null }
$Template = Get-Content (Join-Path $Root 'packaging\windows\AppxManifest.xml.in') -Raw
$AssetsSrc = Join-Path $Root 'packaging\windows\msix-assets'
if (-not (Test-Path $AssetsSrc)) { throw "missing $AssetsSrc (python scripts/make-icons.py --msix-only)" }
$esc = { param($s) [System.Security.SecurityElement]::Escape($s) }

# --- one package per architecture ---------------------------------------------------------------
$BundleIn = Join-Path $Work 'bundle-in'
New-Item -ItemType Directory -Path $BundleIn | Out-Null
foreach ($arch in $Archs) {
    $stage = Join-Path $Root "dist\Siple-$StageVersion-windows-$arch-store"
    foreach ($f in 'siple.exe', 'siple-cli.exe') {
        if (-not (Test-Path (Join-Path $stage $f))) { throw "missing $stage\$f (run package-windows.ps1 -Arch $arch -StageOnly -Store)" }
    }
    $layout = Join-Path $Work "layout-$arch"
    New-Item -ItemType Directory -Path (Join-Path $layout 'Assets') -Force | Out-Null
    foreach ($f in 'siple.exe', 'siple-cli.exe', 'LICENSE', 'THIRD-PARTY-NOTICES.html') {
        $p = Join-Path $stage $f
        if (Test-Path $p) { Copy-Item $p $layout }
        elseif ($f -ne 'THIRD-PARTY-NOTICES.html') { throw "missing $p" }
        else { Write-Warning "no THIRD-PARTY-NOTICES.html in $stage" }
    }
    Copy-Item (Join-Path $AssetsSrc '*.png') (Join-Path $layout 'Assets')
    $manifest = $Template.
        Replace('@IDENTITY_NAME@', (& $esc $IdentityName)).
        Replace('@PUBLISHER@', (& $esc $Publisher)).
        Replace('@VERSION@', $MsixVersion).
        Replace('@ARCH@', $arch).
        Replace('@DISPLAY_NAME@', (& $esc $DisplayName)).
        Replace('@PUBLISHER_DISPLAY_NAME@', (& $esc $PublisherDisplayName))
    if ($manifest -match '@[A-Z_]+@') { throw "unreplaced token in manifest: $($Matches[0])" }
    [System.IO.File]::WriteAllText((Join-Path $layout 'AppxManifest.xml'), $manifest, (New-Object System.Text.UTF8Encoding($false)))

    Write-Host "==> [$arch] makepri"
    $cfg = Join-Path $Work "priconfig-$arch.xml"
    Invoke-Tool $MakePri @('createconfig', '/cf', $cfg, '/dq', 'en-US', '/pv', '10.0.0', '/o') | Out-Null
    Invoke-Tool $MakePri @('new', '/pr', $layout, '/cf', $cfg, '/mn', (Join-Path $layout 'AppxManifest.xml'),
        '/of', (Join-Path $layout 'resources.pri'), '/o') | Out-Null
    if (-not (Test-Path (Join-Path $layout 'resources.pri'))) { throw "[$arch] makepri produced no resources.pri" }

    $msix = Join-Path $BundleIn "Siple_${MsixVersion}_$arch.msix"
    Write-Host "==> [$arch] makeappx pack"
    Invoke-Tool $MakeAppx @('pack', '/d', $layout, '/p', $msix, '/h', 'SHA256', '/o') | Out-Null
    Write-Host ("    {0} ({1:N1} MB)" -f (Split-Path -Leaf $msix), ((Get-Item $msix).Length / 1MB))
}

# --- bundle ---------------------------------------------------------------------------------------
$BundleName = "Siple_${MsixVersion}_$($Archs -join '_').msixbundle"
$Bundle = Join-Path $Out $BundleName
Write-Host "==> makeappx bundle $MsixVersion"
Invoke-Tool $MakeAppx @('bundle', '/d', $BundleIn, '/p', $Bundle, '/bv', $MsixVersion, '/o') | Out-Null

# --- validate ----------------------------------------------------------------------------------------
Write-Host '==> validate bundle'
$vb = Join-Path $Work 'unpack-bundle'
Invoke-Tool $MakeAppx @('unbundle', '/p', $Bundle, '/d', $vb, '/o') | Out-Null
[xml]$bm = Get-Content (Join-Path $vb 'AppxMetadata\AppxBundleManifest.xml') -Raw
$ns = New-Object System.Xml.XmlNamespaceManager($bm.NameTable)
$ns.AddNamespace('b', $bm.DocumentElement.NamespaceURI)
$bid = $bm.SelectSingleNode('/b:Bundle/b:Identity', $ns)
if ($bid.Name -ne $IdentityName -or $bid.Publisher -ne $Publisher -or $bid.Version -ne $MsixVersion) {
    throw "bundle identity mismatch: $($bid.Name) / $($bid.Publisher) / $($bid.Version)"
}
$apps = @($bm.SelectNodes('/b:Bundle/b:Packages/b:Package[@Type="application"]', $ns))
foreach ($arch in $Archs) {
    $p = $apps | Where-Object { $_.Architecture -eq $arch }
    if (-not $p) { throw "bundle manifest has no $arch application package" }
    if ($p.Version -ne $MsixVersion) { throw "bundle manifest: $arch package version is $($p.Version), expected $MsixVersion" }
}
if ($apps.Count -ne $Archs.Count) { throw "bundle lists $($apps.Count) application packages, expected $($Archs.Count)" }
foreach ($arch in $Archs) {
    $inner = Get-ChildItem $vb -Recurse -File -Filter "*$arch*.msix" | Select-Object -First 1
    if (-not $inner) { $inner = Get-Item (Join-Path $BundleIn "Siple_${MsixVersion}_$arch.msix") }
    $vi = Join-Path $Work "unpack-$arch"
    Invoke-Tool $MakeAppx @('unpack', '/p', $inner.FullName, '/d', $vi, '/o') | Out-Null
    [xml]$mx = Get-Content (Join-Path $vi 'AppxManifest.xml') -Raw
    $id = $mx.Package.Identity
    if ($id.Name -ne $IdentityName -or $id.Publisher -ne $Publisher -or $id.Version -ne $MsixVersion -or $id.ProcessorArchitecture -ne $arch) {
        throw "[$arch] inner identity mismatch: $($id.Name) / $($id.Publisher) / $($id.Version) / $($id.ProcessorArchitecture)"
    }
    if (-not (Test-Path (Join-Path $vi 'resources.pri'))) { throw "[$arch] resources.pri missing in the package" }
    $protocols = @($mx.SelectNodes("//*[local-name()='Protocol']") | ForEach-Object { $_.GetAttribute('Name') })
    foreach ($scheme in 'sip', 'sips', 'tel', 'callto', 'siple') {
        if ($protocols -notcontains $scheme) { throw "[$arch] the manifest does not declare the $scheme protocol" }
    }
    foreach ($exe in 'siple.exe', 'siple-cli.exe') {
        $m = Get-PeMachine (Join-Path $vi $exe)
        if ($m -ne $PeMachine[$arch]) { throw "[$arch] $exe machine type is $m, expected $($PeMachine[$arch])" }
    }
    Write-Host "    [$arch] identity, version, resources.pri, PE machine OK"
}
Write-Host "==> $Bundle"
Get-FileHash $Bundle -Algorithm SHA256 | ForEach-Object { Write-Host ("    sha256 " + $_.Hash) }

# --- test-signed sideload copy -----------------------------------------------------------------------
if ($TestSign) {
    $signtool = Find-SdkTool 'signtool.exe'
    $side = Join-Path $Out 'sideload'
    New-Item -ItemType Directory -Path $side | Out-Null
    $signed = Join-Path $side ($BundleName -replace '\.msixbundle$', '_test.msixbundle')
    Copy-Item $Bundle $signed
    Write-Host "==> test-signing a copy with a throwaway certificate ($Publisher)"
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher -KeyUsage DigitalSignature `
        -FriendlyName 'Siple test signing (throwaway)' -CertStoreLocation 'Cert:\LocalMachine\My' `
        -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}') -NotAfter (Get-Date).AddMonths(3)
    $thumb = $cert.Thumbprint
    try {
        Export-Certificate -Cert $cert -FilePath (Join-Path $side 'siple-test.cer') | Out-Null
        Invoke-Tool $signtool @('sign', '/fd', 'SHA256', '/sm', '/s', 'My', '/sha1', $thumb, $signed) | Out-Null
    } finally {
        Remove-Item "Cert:\LocalMachine\My\$thumb" -DeleteKey -Force -ErrorAction SilentlyContinue   # deletes the private key
    }
    Copy-Item (Join-Path $PSScriptRoot 'install-test.ps1') $side
    Set-Content -Path (Join-Path $side 'identity.txt') -Value $IdentityName -Encoding ASCII
    @"
Siple sideload test package (NOT for the Store).

  1. Open an elevated PowerShell in this folder.
  2. .\install-test.ps1            imports siple-test.cer into LocalMachine\TrustedPeople and installs the bundle
                                    (-ForceUpdateFromAnyVersion: updating from another test build keeps LocalState).
  3. siple-cli --version           works from any new terminal (app execution alias).
  4. .\install-test.ps1 -Uninstall removes the package AND the test certificate.

Identity: $IdentityName / $Publisher, version $MsixVersion. The signature is self-made: it only works on machines
that trust siple-test.cer. Uninstalling the package deletes its LocalState (config, logs).
"@ | Set-Content -Path (Join-Path $side 'README.txt') -Encoding ASCII
    $sig = Get-AuthenticodeSignature $signed
    if ($sig.SignerCertificate.Thumbprint -ne $thumb) { throw 'test-signed bundle does not carry the expected signature' }
    Write-Host "==> sideload bundle: $signed (cert key deleted; public cert: siple-test.cer)"
}
Write-Host '==> done'
