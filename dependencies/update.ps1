# Zona Installer self-update script.
#
# Invoked by zona-installer.exe when a newer release is available:
#   update.ps1 -InstallerPid <pid> -Url <zip-url> -Dest <zona-dir> -Relaunch "<flags>"
#
# The old installer has already exited (the hand-off exits immediately), but the
# script still waits on the process id so the running exe's file lock is fully
# released before the zip is extracted over it.

param(
    [Parameter(Mandatory = $true)][int]$InstallerPid,
    [Parameter(Mandatory = $true)][string]$Url,
    [Parameter(Mandatory = $true)][string]$Dest,
    [string]$Relaunch = ""
)

$ErrorActionPreference = "Stop"
Write-Host "Updating Zona installer..."

# 1. Wait for the old installer to fully exit, releasing its exe lock.
try {
    Wait-Process -Id $InstallerPid -ErrorAction Stop
} catch {
    # The process is already gone: nothing to wait for.
}

# 2. Download the release zip into ..\downloads.
$installer = Join-Path $PSScriptRoot ".."
$downloads = Join-Path $installer "downloads"
New-Item -ItemType Directory -Force -Path $downloads | Out-Null
$zip = Join-Path $downloads "zona-installer.zip"
Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing

# 3. Extract over Zona (overwrite; the old exe is gone so this succeeds).
Expand-Archive -Path $zip -DestinationPath $Dest -Force

# 4. Verify the new installer exists before launching it.
$newExe = Join-Path $Dest "installer\zona-installer.exe"
if (-not (Test-Path -LiteralPath $newExe)) {
    Write-Error "Update failed: $newExe was not found after extraction."
    exit 1
}

# 5. Relaunch the new installer with the stored flags.
if ($Relaunch) {
    Start-Process -FilePath $newExe -ArgumentList $Relaunch
} else {
    Start-Process -FilePath $newExe
}
