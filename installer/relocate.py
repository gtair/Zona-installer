"""
Relocation module for Zona Installer.
Ensures installer runs from [drive]:/Zona/installer
"""

import shutil
import subprocess
import sys
import ctypes
from pathlib import Path
from tkinter import messagebox, ttk
import tkinter as tk

from log_custom import log


def get_local_drives():
    """Get list of local drive letters (Windows only).
    Excludes network and other virtual drives."""
    drives = []
    bitmask = ctypes.windll.kernel32.GetLogicalDrives()
    for i in range(26):
        if bitmask & (1 << i):
            drive_letter = chr(ord('A') + i)
            try:
                # Check if it's a fixed drive (local disk)
                drive_type = ctypes.windll.kernel32.GetDriveTypeW(f"{drive_letter}:/")
                # DRIVE_FIXED = 3
                if drive_type == 3:
                    drives.append(drive_letter)
            except Exception as e:
                log("debug", f"Could not get type for drive {drive_letter}: {e}")
    return sorted(drives)


def ask_drive_letter():
    """Show dialog asking user which local drive to install Zona on.
    Returns Path like 'D:/Zona/installer' or None if cancelled."""
    local_drives = get_local_drives()
    
    if not local_drives:
        messagebox.showerror(
            "No Local Drives Found",
            "Could not find any local fixed drives on this system."
        )
        return None
    
    root = tk.Tk()
    root.title("Zona Installer - Drive Selection")
    root.resizable(False, False)
    root.geometry("500x350")
    
    frame = ttk.Frame(root, padding=15)
    frame.pack(fill="both", expand=True)
    
    # Title and warning
    ttk.Label(
        frame,
        text="Installation Directory Required",
        font=("Segoe UI", 12, "bold")
    ).pack(anchor="w", pady=(0, 10))
    
    warning_text = (
        "Please select one of the available local drives where Zona will be installed. "
        "The installer will create a 'Zona' folder on that drive, move itself to that location and install the game there."
    )
    ttk.Label(
        frame,
        text=warning_text,
        foreground="#666666",
        wraplength=450,
        justify="left"
    ).pack(anchor="w", pady=(0, 20))
    
    # Drive selection
    ttk.Label(
        frame,
        text="Select a local drive:",
        font=("Segoe UI", 10)
    ).pack(anchor="w", pady=(0, 10))
    
    drive_var = tk.StringVar(value=local_drives[0] if local_drives else "")
    
    for drive_letter in local_drives:
        ttk.Radiobutton(
            frame,
            text=f"{drive_letter}:/",
            value=drive_letter,
            variable=drive_var
        ).pack(anchor="w")
    
    # Buttons
    button_frame = ttk.Frame(frame)
    button_frame.pack(fill="x", pady=(20, 0))
    
    result = {"cancelled": True}
    
    def on_ok():
        result["cancelled"] = False
        root.destroy()
    
    def on_cancel():
        result["cancelled"] = True
        root.destroy()
    
    ttk.Button(button_frame, text="OK", command=on_ok).pack(side="right", padx=(5, 0))
    ttk.Button(button_frame, text="Cancel", command=on_cancel).pack(side="right")
    
    root.mainloop()
    
    if result["cancelled"]:
        return None
    
    drive = drive_var.get()
    target_path = Path(f"{drive}:/Zona")
    
    # Check if target already exists
    if target_path.exists():
        messagebox.showerror(
            "Directory Already Exists",
            f"{target_path} already exists.\n"
            "Please delete it or contact support if you are unsure."
        )
        log("error", f"Target directory already exists: {target_path}")
        return None
    
    return target_path / "installer"


def copy_installer(source_dir, target_dir):
    """Copy entire installer directory to target location.
    
    Args:
        source_dir: Path to current installer directory
        target_dir: Path to target installer directory (should not exist)
    
    Returns:
        True if successful, False otherwise
    """
    try:
        log("info", f"Copying installer from {source_dir} to {target_dir}")
        
        # Ensure parent directory exists
        target_dir.parent.mkdir(parents=True, exist_ok=True)
        
        # Copy entire tree
        shutil.copytree(source_dir, target_dir, dirs_exist_ok=False)
        
        log("info", f"Successfully copied installer to {target_dir}")
        return True
    except Exception as e:
        log("error", f"Failed to copy installer: {e}")
        return False


def handle_deletion_argument(folder_to_delete):
    """Handle /delete argument by deleting the specified installer folder only.
    Note: The 1-second wait is handled in main.py before calling this.
    
    Args:
        folder_to_delete: Path to installer folder to delete
    """
    try:
        folder_path = Path(folder_to_delete)
        
        log("info", f"Deletion mode activated for {folder_path}")
        
        if not folder_path.exists():
            log("warning", f"Folder {folder_path} does not exist, skipping deletion")
            return True
        
        log("info", f"Deleting {folder_path}...")
        shutil.rmtree(folder_path)
        
        log("info", f"Successfully deleted {folder_path}")
        return True
    except Exception as e:
        log("error", f"Failed to delete {folder_path}: {e}")
        return False


def check_and_relocate():
    """Check if installer is in correct location. If not, relocate it.
    
    Returns:
        (should_continue, target_dir) tuple where:
        - should_continue: True if we should continue execution, False if we should exit
        - target_dir: Path to the current/correct installer directory
    """
    current_dir = Path(__file__).parent
    current_drive = current_dir.drive.rstrip(":")
    expected_root = Path(f"{current_drive}:/Zona/installer")
    
    # Normalize paths for comparison (resolve symlinks, etc.)
    try:
        current_normalized = current_dir.resolve()
        expected_normalized = expected_root.resolve()
    except Exception as e:
        log("error", f"Error resolving paths: {e}")
        current_normalized = current_dir
        expected_normalized = expected_root
    
    # If we're already in the right place, continue normally
    if str(current_normalized).lower() == str(expected_normalized).lower():
        log("info", f"Installer is in correct location: {current_dir}")
        return True, current_dir
    
    log("warning", f"Installer not in correct location. Current: {current_dir}, Expected: {expected_root}")
    
    # Ask user where they want Zona installed
    target_dir = ask_drive_letter()
    
    if target_dir is None:
        log("error", "User cancelled relocation")
        return False, None
    
    # Copy everything to target location
    if not copy_installer(current_dir, target_dir):
        messagebox.showerror("Installation Failed", "Could not copy installer to target location. Check logs for details.")
        return False, None
    
    # Show success message
    messagebox.showinfo(
        "Relocation Complete",
        f"Installer has been copied to {target_dir}\n\n"
        "A new instance will now launch from the correct location.\n"
        "The old copy will be automatically deleted."
    )
    
    # Launch new instance from target location with /delete argument for current location
    try:
        new_python = Path(target_dir) / "dependencies" / "python" / "pythonw.exe"
        new_main = Path(target_dir) / "main.pyw"

        log("info", f"Launching new instance from {target_dir}")

        # Fully detach process on Windows
        flags = (
            subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP
            if sys.platform == "win32"
            else 0
        )

        subprocess.Popen(
            [str(new_python), str(new_main), "/delete", str(current_dir)],
            cwd=str(target_dir),  # CRITICAL: Unlocks current_dir so it can be deleted
            creationflags=flags
        )
        log("info", "New instance launched, exiting current instance")
    except Exception as e:
        log("error", f"Failed to launch new instance: {e}")
        messagebox.showerror("Launch Failed", f"Could not launch new instance: {e}")
        return False, None
    
    # Exit this instance
    return False, None
