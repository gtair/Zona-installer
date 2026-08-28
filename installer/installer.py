"""Runs the ordered install steps described in config.yaml against downloaded assets.

Everything here is plain filesystem work - no Tkinter, no networking. The one step type
this module refuses to run is "verify_game", since launching the game and watching the
user close it is a GUI concern that belongs in main.py.
"""

import re
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path
from typing import Callable, Optional

import multivolumefile
import py7zr

from log_custom import log
from win_job import close_job, create_kill_on_close_job

ROOT = Path(__file__).parent

StepCallback = Optional[Callable[[dict], None]]
ProgressCallback = Optional[Callable[[int, Optional[str]], None]]

_SEVEN_ZIP_CANDIDATES = [
    r"dependencies/7z.exe",  # portable 7z in project dependencies
    r"C:\Program Files\7-Zip\7z.exe",
    r"C:\Program Files (x86)\7-Zip\7z.exe",
]
_PERCENT_RE = re.compile(r"(\d{1,3})%")


def _looks_like_member_index(value: str) -> bool:
    return bool(value) and value.replace(" ", "").isdigit()


def _parse_seven_zip_progress_line(line: str) -> tuple[Optional[int], Optional[str]]:
    """Parse 7z output such as '40%  test_folder\test_file.pak', '40% - test_folder/test_file.pak',
    or the real extracted-file line '- a.txt' that 7z emits after the percent update.

    Ignore numeric-only member indexes like '26', which 7z emits in some archive layouts but are
    not the target filename that users care about.
    """
    text = line.strip()
    if not text:
        return None, None

    if text.startswith("- "):
        detail = text[2:].strip().replace("\\", "/")
        if _looks_like_member_index(detail):
            return None, None
        return None, detail or None

    match = re.search(r"(\d{1,3})%\s*(?:-\s*)?(.*)$", text)
    if not match:
        return None, None

    percent = int(match.group(1))
    detail = match.group(2).strip()
    if not detail:
        return percent, None

    detail = detail.replace("\\", "/")
    if detail.startswith("-"):
        detail = detail[1:].strip()
    if _looks_like_member_index(detail):
        return percent, None
    return percent, detail or None


def _find_seven_zip() -> Optional[str]:
    return shutil.which("7z") or next((p for p in _SEVEN_ZIP_CANDIDATES if Path(p).exists()), None)


SEVEN_ZIP_EXE = _find_seven_zip()
SHOW_7Z_CONSOLE = False
log("info", f"7z.exe {'found at ' + SEVEN_ZIP_EXE if SEVEN_ZIP_EXE else 'not found - falling back to py7zr'}")


def set_show_7z_console(enabled: bool) -> None:
    global SHOW_7Z_CONSOLE
    SHOW_7Z_CONSOLE = bool(enabled)
    log("info", f"7z console visibility set to {SHOW_7Z_CONSOLE}")


def create_dirs(names: list[str]) -> None:
    for name in names:
        path = ROOT / name
        log("debug", f"Ensuring directory exists: {path}")
        path.mkdir(parents=True, exist_ok=True)


def _seven_zip_extract_args(archive_path: Path, target_dir: Path, on_progress: bool = False) -> list[str]:
    """Build the explicit 7z extraction command. Overwrite-all is intentional here."""
    args = [
        SEVEN_ZIP_EXE,
        "x",
        str(archive_path),
        f"-o{target_dir}",
        "-y",
        "-aoa",
        "-bb1",
    ]
    if on_progress:
        args.append("-bsp1")
    return args


def _run_seven_zip(archive_path: Path, target_dir: Path, on_progress: ProgressCallback = None) -> None:
    """Run 7z.exe bound to a kill-on-close job object, so it can't survive as an orphan
    holding file handles on the install folder if we get closed mid-extraction."""
    capture_output = on_progress is not None
    args = _seven_zip_extract_args(archive_path, target_dir, capture_output)

    kwargs = {
        "text": True,
    }
    if capture_output:
        kwargs["stdout"] = subprocess.PIPE
        kwargs["stderr"] = subprocess.DEVNULL
    else:
        kwargs["stdout"] = None
        kwargs["stderr"] = None

    if SHOW_7Z_CONSOLE:
        kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_CONSOLE", 0)

    process = subprocess.Popen(args, **kwargs)
    job = create_kill_on_close_job(process)
    try:
        if on_progress:
            _watch_seven_zip_progress(process, on_progress)
        return_code = process.wait()
    finally:
        close_job(job)
    if return_code != 0:
        raise subprocess.CalledProcessError(return_code, process.args)


def _watch_seven_zip_progress(process: subprocess.Popen, on_progress: Callable[[int, Optional[str]], None]) -> None:
    """7z.exe emits file-name lines and percent lines in a paired order, not necessarily the same order
    the UI expects. Keep the last real filename and then combine it with the following percent update.
    """
    buffer = ""
    current_percent = None
    current_detail = None
    last_detail = None

    while True:
        chunk = process.stdout.read(1)
        if chunk == "":
            if process.poll() is not None:
                break
            continue
        if chunk in ("\r", "\n"):
            line = buffer.strip()
            percent, detail = _parse_seven_zip_progress_line(line)

            if detail is not None and not _looks_like_member_index(detail):
                last_detail = detail

            if percent is not None:
                current_percent = min(percent, 100)
                current_detail = last_detail
                on_progress(current_percent, current_detail)

            buffer = ""
        else:
            buffer += chunk


def _extract_zip_filtered(archive_path: Path, target_dir: Path, from_paths: list[str]) -> None:
    """Extract only specified paths from a ZIP file, flattening them to target_dir."""
    log("debug", f"{archive_path.name}: extracting filtered paths {from_paths}")
    from_paths_normalized = [p.replace("\\", "/") for p in from_paths]
    
    with zipfile.ZipFile(archive_path) as archive:
        for member in archive.namelist():
            member_normalized = member.replace("\\", "/")
            # Check if this member is under any of the from_paths
            for from_path in from_paths_normalized:
                if member_normalized.startswith(from_path + "/") or member_normalized == from_path:
                    # Extract with the prefix removed
                    relative_path = member_normalized[len(from_path):].lstrip("/")
                    if relative_path:  # Skip the directory itself
                        target_file = target_dir / relative_path
                        target_file.parent.mkdir(parents=True, exist_ok=True)
                        with archive.open(member) as source:
                            with open(target_file, "wb") as target:
                                target.write(source.read())
                    break


def _move_filtered_contents(temp_dir: Path, target_dir: Path, from_paths: list[str]) -> None:
    """Move contents of specified paths from temp_dir to target_dir."""
    for from_path in from_paths:
        source_dir = temp_dir / from_path
        if not source_dir.exists():
            log("warn", f"Path not found in archive: {from_path}")
            continue
        
        log("debug", f"Moving contents of {from_path} to {target_dir}")
        for item in source_dir.iterdir():
            target_item = target_dir / item.name
            if item.is_dir():
                if target_item.exists():
                    shutil.rmtree(target_item)
                shutil.move(str(item), str(target_item))
            else:
                target_item.parent.mkdir(parents=True, exist_ok=True)
                shutil.move(str(item), str(target_item))


def extract_archive(archive_path: Path, target_dir: Path, from_paths: Optional[list[str]] = None, on_progress: ProgressCallback = None) -> None:
    """Extract a .zip, single-volume .7z, or multi-volume .7z.001+ archive.

    Multi-volume archives are detected by a numeric suffix (.001, .002, ...) and must be
    pointed at their first part - the remaining parts are expected to sit alongside it.
    Prefers a real 7z.exe when one is installed - py7zr's pure-python decompression is
    far too slow for multi-gigabyte modpacks to be practical as the only path. Only the
    7z.exe path reports live percentages; py7zr/zipfile just report done-or-not.
    
    Args:
        archive_path: Path to the archive file
        target_dir: Directory to extract to
        from_paths: Optional list of paths to extract (e.g., ['clean_hud/appdata', 'clean_hud/gamedata'])
                   If specified, only these paths are extracted and their contents are placed at target_dir root
        on_progress: Optional callback for progress updates
    """
    target_dir.mkdir(parents=True, exist_ok=True)
    log("info", f"Extracting {archive_path.name} -> {target_dir}" + (f" (from: {from_paths})" if from_paths else ""))

    if archive_path.suffix == ".zip":
        log("debug", f"{archive_path.name}: using zipfile")
        if from_paths:
            _extract_zip_filtered(archive_path, target_dir, from_paths)
        else:
            with zipfile.ZipFile(archive_path) as archive:
                archive.extractall(target_dir)
        log("debug", f"{archive_path.name}: zipfile extraction done")
        return

    if SEVEN_ZIP_EXE:
        log("debug", f"{archive_path.name}: using {SEVEN_ZIP_EXE}")
        if from_paths:
            # For 7z with from_paths, extract to temp dir then move filtered contents
            with tempfile.TemporaryDirectory() as temp_dir:
                _run_seven_zip(archive_path, Path(temp_dir), on_progress=on_progress)
                _move_filtered_contents(Path(temp_dir), target_dir, from_paths)
        else:
            _run_seven_zip(archive_path, target_dir, on_progress=on_progress)
        log("debug", f"{archive_path.name}: 7z.exe extraction done")
        return

    if archive_path.suffix.lstrip(".").isdigit():
        base_path = archive_path.with_suffix("")
        log("debug", f"{archive_path.name}: using py7zr multivolume, base={base_path.name}")
        if from_paths:
            # For py7zr with from_paths, extract to temp dir then move filtered contents
            with tempfile.TemporaryDirectory() as temp_dir:
                temp_path = Path(temp_dir)
                with multivolumefile.open(base_path, mode="rb") as volumes:
                    with py7zr.SevenZipFile(volumes, mode="r") as archive:
                        archive.extractall(path=temp_path)
                _move_filtered_contents(temp_path, target_dir, from_paths)
        else:
            with multivolumefile.open(base_path, mode="rb") as volumes:
                with py7zr.SevenZipFile(volumes, mode="r") as archive:
                    archive.extractall(path=target_dir)
        log("debug", f"{archive_path.name}: py7zr multivolume extraction done")
        return

    log("debug", f"{archive_path.name}: using py7zr")
    if from_paths:
        # For py7zr with from_paths, extract to temp dir then move filtered contents
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            with py7zr.SevenZipFile(archive_path, mode="r") as archive:
                archive.extractall(path=temp_path)
            _move_filtered_contents(temp_path, target_dir, from_paths)
    else:
        with py7zr.SevenZipFile(archive_path, mode="r") as archive:
            archive.extractall(path=target_dir)
    log("debug", f"{archive_path.name}: py7zr extraction done")


def _selected_assets(assets: list[dict], selected_choice_ids: set[str]) -> list[dict]:
    return [asset for asset in assets if asset["choice"] in selected_choice_ids]


def _run_extract_step(step: dict, ctx: dict, on_progress: ProgressCallback = None) -> None:
    from_paths = step.get("from", None)
    if "file" in step:
        extract_archive(ROOT / step["file"], ROOT / step["target"], from_paths=from_paths, on_progress=on_progress)
        return

    # steps without a "file" install whatever optional assets the user picked
    selected = _selected_assets(ctx["assets"], ctx["selected_choice_ids"])
    log("debug", f"{step['name']}: {len(selected)} selected asset(s) match {ctx['selected_choice_ids']}")
    for asset in selected:
        extract_archive(ROOT / "downloads" / asset["file"], ROOT / step["target"], from_paths=from_paths, on_progress=on_progress)


def _run_delete_step(step: dict, ctx: dict, on_progress: ProgressCallback = None) -> None:
    """Delete files or directories specified in 'dir' or 'file' keys."""
    paths_to_delete = []
    
    if "dir" in step:
        dirs = step["dir"] if isinstance(step["dir"], list) else [step["dir"]]
        paths_to_delete.extend(dirs)
    
    if "file" in step:
        files = step["file"] if isinstance(step["file"], list) else [step["file"]]
        paths_to_delete.extend(files)
    
    for path_str in paths_to_delete:
        path = ROOT / path_str
        if not path.exists():
            log("debug", f"Path does not exist (skipping): {path}")
            continue
        
        try:
            if path.is_dir():
                log("info", f"Deleting directory: {path}")
                shutil.rmtree(path)
            else:
                log("info", f"Deleting file: {path}")
                path.unlink()
        except Exception as e:
            log("error", f"Failed to delete {path}: {e}")
            raise


STEP_HANDLERS: dict[str, Callable[[dict, dict, ProgressCallback], None]] = {
    "create_dir": lambda step, ctx, on_progress: create_dirs(step["dir"]),
    "extract": _run_extract_step,
    "delete": _run_delete_step,
}


def run_steps(
    steps: list[dict],
    ctx: dict,
    on_step_start: StepCallback = None,
    on_step_progress: Optional[Callable[[dict, int], None]] = None,
    on_step_done: StepCallback = None,
) -> None:
    """Run a slice of steps in order. Raises on the first failure - callers decide how
    to surface that to the user. main.py never passes a slice containing verify_game;
    this skip is just a safety net.
    """
    for step in steps:
        if step["action"] == "verify_game":
            log("debug", f"Skipping {step['name']} (verify_game is handled by main.py)")
            continue

        log("info", f"Step start: {step['name']} ({step['action']})")
        if on_step_start:
            on_step_start(step)
        progress_cb = (lambda percent, detail=None, s=step: on_step_progress(s, percent, detail)) if on_step_progress else None
        try:
            STEP_HANDLERS[step["action"]](step, ctx, progress_cb)
        except Exception:
            log("error", f"Step failed: {step['name']}")
            raise
        log("info", f"Step done: {step['name']}")
        if on_step_done:
            on_step_done(step)
