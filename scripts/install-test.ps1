<#
.SYNOPSIS
  Installs (or removes) the test-signed Siple sideload bundle from this folder. Run elevated.

.DESCRIPTION
  Imports siple-test.cer into LocalMachine\TrustedPeople, then Add-AppxPackage -ForceUpdateFromAnyVersion
  (an update from another test build keeps LocalState; uninstalling deletes it).
  -Uninstall removes the package and the test certificate from TrustedPeople, My and CA (Windows copies a
  sideload signing certificate into CA as well).
#>
param([switch]$Uninstall, [string]$Bundle, [string]$Cert)

$ErrorActionPreference = 'Stop'
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'run this from an elevated PowerShell (certificate import into LocalMachine)'
}
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Cert) { $Cert = Join-Path $here 'siple-test.cer' }
if (-not $Bundle) {
    $Bundle = Get-ChildItem $here -Filter '*_test.msixbundle' | Select-Object -First 1 -ExpandProperty FullName
}
if (-not (Test-Path $Cert)) { throw "certificate not found: $Cert" }
$idFile = Join-Path $here 'identity.txt'
$IdentityName = if (Test-Path $idFile) { (Get-Content $idFile -Raw).Trim() } else { 'io.zyno.siple' }
$thumb = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($Cert)).Thumbprint

if ($Uninstall) {
    $pkg = Get-AppxPackage -Name $IdentityName
    foreach ($p in @($pkg)) {
        if ($p.SignatureKind -eq 'Developer' -or $p.SignatureKind -eq 'None') {
            Write-Host "==> removing $($p.PackageFullName)"
            Remove-AppxPackage -Package $p.PackageFullName
        } else {
            Write-Warning "leaving $($p.PackageFullName) (SignatureKind $($p.SignatureKind): not a sideload test install)"
        }
    }
    foreach ($store in 'TrustedPeople', 'My', 'CA') {
        Get-ChildItem "Cert:\LocalMachine\$store" | Where-Object { $_.Thumbprint -eq $thumb } |
            ForEach-Object { Remove-Item $_.PSPath -Force; Write-Host "==> removed test certificate from LocalMachine\$store" }
    }
    return
}

if (-not $Bundle -or -not (Test-Path $Bundle)) { throw 'no *_test.msixbundle next to this script (use -Bundle)' }
Write-Host "==> trusting $Cert (LocalMachine\TrustedPeople)"
Import-Certificate -FilePath $Cert -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null
Write-Host "==> installing $Bundle"
Add-AppxPackage -Path $Bundle -ForceUpdateFromAnyVersion
Get-AppxPackage -Name $IdentityName | Format-List Name, PackageFullName, Version, Architecture, SignatureKind, InstallLocation
Write-Host 'siple-cli --version works from a NEW terminal. Remove with: .\install-test.ps1 -Uninstall'
