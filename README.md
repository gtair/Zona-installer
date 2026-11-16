# Zona Installer

Installs the full Zona modpack in one go and keeps it updated. WINDOWS ONLY!

- [Download](https://github.com/gtair/Zona-installer/releases/latest)
- [Website](https://stalker-zona.com)
- [Discord](https://discord.gg/zona)

## Requirements

- Windows, 64 bit
- 80 GB free (58 GB installed, 22 GB of downloads you can delete at the end)
- Administrator

## Installation

> [!TIP]
> watch [this video](https://www.youtube.com/watch?v=3l3X_MNMb3w&) if you are too lazy to read

1. Download the zip from the latest release and extract it.
2. Run `zona-installer.exe`.
3. Windows will possibly warn you. See [antivirus and SmartScreen](#antivirus-and-smartscreen), then accept the UAC prompt.
4. Pick the drive. It installs to `<drive>:\Zona` and moves itself there, so the folder you extracted is temporary.
5. Pick what you want and confirm.
6. Wait for the downloads.
7. Partway through it asks you to launch the game. Do that, get to the main menu and check for `S.T.A.L.K.E.R. Anomaly 1.5.3 Modded Exes` in the bottom left. Don't load a save. Exit to windows, then continue.
8. Done. You can delete the 22 GB of downloads now.

You only download the installer once, it updates itself after that. The launcher gives you `Launch installer`, `Launch Mod Organizer` and `Launch Zona`.

## Options

| Option | What it does |
|---|---|
| **Install Zona** | Fresh install: Anomaly, MO2, Zona, HUD, graphics profile |
| **Update Graphics** | Changes SSS21/SSS23 or HUD without reinstalling the modpack |
| **Update Zona** | Updates to the newest Zona version |

`SSS21` is the performance profile, `SSS23` is quality and the default. Both HUDs are cosmetic.

> [!WARNING]
> **Update Zona** replaces `mo2\profiles` and `mo2\mods`. Your saves are safe, they're not in there, but mods you added yourself and any custom load order are gone.
> Moving over from a manual install needs manual setup, so ask in the discord first.

## Updating

It checks github for a newer release every time you launch it. The update downloads, verifies the PGP signature and restarts the installer. A release that doesn't verify is refused, so a failed update leaves your install untouched.

## Troubleshooting

### Antivirus and SmartScreen

The installer isn't signed with a commercial certificate, so Windows usually warns you first. It's purely reputation based and goes away as more people download a release. Click **More info → Run anyway** and accept the UAC prompt.

Some AVs also quarantine files the installer downloads, sometimes without telling you. If a download or an install step fails for no obvious reason, check the quarantine first.

### Downloads stuck or Failed

- `anomaly.7z` is a torrent. A VPN or ISP that blocks torrent traffic will stall it with no useful error.
- `aria2c` retries and resumes. A `.aria2` file next to a download means it's still incomplete.
- Anything that's already finished is hash checked and skipped, so just running it again is safe.
- Wrong URL or reason: `<drive>:\Zona\installer\logs\aria2c.log`

> [!TIP]
> If `anomaly.7z` won't come down, get the official Anomaly 1.5.3 full archive from [moddb](https://www.moddb.com/mods/stalker-anomaly/downloads/stalker-anomaly-153), put it in `<drive>:\Zona\installer\downloads` and name it `anomaly.7z`. The installer hash checks it and only re-downloads if it doesn't match.

### Installation failed

The popup shows an error id and the log path. Post both in the discord.

| Error id | Meaning |
|---|---|
| `E_IO` | File operation failed: permissions, disk full, or a locked file |
| `E_HASH_MISMATCH` | A download didn't match its SHA-256, it's corrupt or truncated |
| `E_CONFIG` | `config.yaml` is invalid |
| `E_TOOL` | `aria2c.exe` or `7za.exe` failed, the message has the exit code |
| `E_WIN32` | Windows API call failed |
| `E_ALREADY_RUNNING` | Another installer window is open |
| `E_NO_DRIVES` | No fixed local drives found |
| `E_TARGET_CONTAINS_INSTALLER` | You pointed it at the folder the installer is running from |
| `E_WORKER_PANIC` | Actual bug. Report it with the log. |

During the install stage you also get **Restart install from scratch**. It doesn't roll anything back, so it's usually cleaner to delete `<drive>:\Zona\mo2` and `<drive>:\Zona\anomaly` first. **Back up your saves before deleting `anomaly`.**

### Hash mismatch

A download is corrupt or truncated. Delete the file from `<drive>:\Zona\installer\downloads` including its `.aria2` file if it has one, and run it again. Also worth checking if your AV quarantined something.

### "The game is not installed yet"

`Launch Mod Organizer` and `Launch Zona` stay disabled until `<drive>:\Zona\mo2\ModOrganizer.exe` exists. If they're greyed out, the install didn't finish, run `Launch installer`.

### The window never appears

Failures before the window opens get written to `%TEMP%\zona-installer-startup-error.txt`. Paste that in your bug report.

### Running out of space

It doesn't check free space for you. If you run out mid install you get a generic `E_IO`, usually while unpacking.

## Linux

There's a full guide on how to install Zona on linux in the discord. There's also an experimental Linux installer made by sickb0y and jmnorman.

## Uninstalling

Delete `<drive>:\Zona` and the `Zona Installer` shortcut. Your saves are in there too, so back them up first if you want to keep them.

## Bug reports

In the discord. Include the error id, the newest log from `<drive>:\Zona\installer\logs` and the relevant `aria2c.log`. Log timestamps are UTC.

## Credits

- Zona modpack by [DrakoMT](https://www.youtube.com/@DrakoMT), installer by [Gtair](https://gtair.dev)
- S.T.A.L.K.E.R. Anomaly by the Anomaly team, modded exes from [xray-monolith](https://github.com/themrdemonized/xray-monolith), Mod Organizer 2 by the MO2 team
- Bundles [7-Zip](https://7-zip.org) and [aria2](https://aria2.github.io)

BSD 2-Clause. See [LICENSE](LICENSE).
