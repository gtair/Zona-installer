param(
    # Optional PGP key id/fingerprint to sign the release zip with.
    [string]$SignKey = ""
)

$version = Read-Host "Enter version (e.g., v0.0.0)"
if ([string]::IsNullOrWhiteSpace($version)) {
    Write-Error "Version cannot be empty."
    exit 1
}

$ProjectRoot = $PSScriptRoot
$releaseDir = Join-Path $ProjectRoot "release"

if (-not (Test-Path $releaseDir)) {
    New-Item -ItemType Directory -Path $releaseDir | Out-Null
}

# The update helper verifies releases against this key at runtime, and the file
# is shipped inside the release zip. Export it with gpg's --output (NOT a
# PowerShell `>` redirect, which would write UTF-16 and break parsing):
#   gpg --armor --export --output dependencies\public-key.asc <keyid>
$publicKey = Join-Path $ProjectRoot "dependencies\public-key.asc"
if (-not (Test-Path $publicKey)) {
    Write-Warning "dependencies\public-key.asc is missing; the update helper will refuse to verify releases."
}

# update.exe is pinned: built once and re-shipped byte-identical every release so
# its SHA-256 (and the Defender/SmartScreen whitelist) stays stable. It is NOT
# rebuilt here. If src\bin\update.rs changes, rebuild it manually
# (cargo build --release --bin update), copy the new binary here, and resubmit
# the new hash to Microsoft.
$pinnedUpdateExe = Join-Path $ProjectRoot "release\builds\update.exe"
if (-not (Test-Path $pinnedUpdateExe)) {
    Write-Error "Pinned update.exe is missing at release\builds\update.exe. Build it once and copy it there."
    exit 1
}

Push-Location $ProjectRoot
cargo build --release --bin zona-installer
$buildExitCode = $LASTEXITCODE
Pop-Location

if ($buildExitCode -ne 0) {
    Write-Error "Cargo build failed."
    exit 1
}

$installerDir = Join-Path $releaseDir "installer"
if (Test-Path $installerDir) {
    Remove-Item -Recurse -Force $installerDir
}
New-Item -ItemType Directory -Path "$installerDir\dependencies" | Out-Null

$sourceExe = Join-Path $ProjectRoot "target\release\zona-installer.exe"
$updateExe = $pinnedUpdateExe
$sourceDeps = Join-Path $ProjectRoot "dependencies\*"

Copy-Item $sourceExe -Destination "$installerDir\"
Copy-Item $updateExe -Destination "$installerDir\dependencies\"
Copy-Item $sourceDeps -Destination "$installerDir\dependencies\" -Recurse

$configFile = Join-Path "$installerDir\dependencies" "config.yaml"
if (Test-Path $configFile) {
    $content = Get-Content $configFile -Raw
    $content = $content -replace '\{\{VERSION\}\}', $version
    Set-Content -Path $configFile -Value $content -NoNewline
    Write-Host "Updated config.yaml with version: $version" -ForegroundColor Cyan
}

$zipName = Join-Path $releaseDir "zona-installer.zip"
if (Test-Path $zipName) {
    Remove-Item $zipName
}
Compress-Archive -Path $installerDir -DestinationPath $zipName

if ($SignKey) {
    $sigPath = "$zipName.asc"
    if (Test-Path $sigPath) {
        Remove-Item $sigPath
    }
    & gpg --armor --detach-sign --local-user $SignKey --output $sigPath $zipName
    if ($LASTEXITCODE -ne 0) {
        Write-Error "gpg signing failed."
        exit 1
    }
    Write-Host "Signed: $sigPath" -ForegroundColor Cyan
}

Write-Host "Success! Created $zipName" -ForegroundColor Green

Write-Host ""
Write-Host "SHA-256 (for Microsoft Threat Intelligence submission):" -ForegroundColor Cyan
$shippedUpdate = Join-Path $installerDir "dependencies\update.exe"
$shippedInstaller = Join-Path $installerDir "zona-installer.exe"
if (Test-Path $shippedUpdate) {
    Write-Host ("  update.exe        " + (Get-FileHash $shippedUpdate -Algorithm SHA256).Hash)
}
if (Test-Path $shippedInstaller) {
    Write-Host ("  zona-installer.exe " + (Get-FileHash $shippedInstaller -Algorithm SHA256).Hash)
}