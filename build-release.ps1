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

Push-Location $ProjectRoot
cargo build --release
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
$sourceDeps = Join-Path $ProjectRoot "dependencies\*"

Copy-Item $sourceExe -Destination "$installerDir\"
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

Write-Host "Success! Created $zipName" -ForegroundColor Green