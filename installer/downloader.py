"""Download batches via the bundled aria2c JSON-RPC client."""

import hashlib
import json
import os
import re
import secrets
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Callable, Iterable, List, Optional
from urllib.parse import urlparse

from log_custom import log
from win_job import close_job, create_kill_on_close_job

ARIA2C_PATH = Path(__file__).parent / "dependencies" / "aria2c.exe"
MAX_HASH_RETRIES = 3


def _is_moddb_url(url: str) -> bool:
    hostname = (urlparse(url).hostname or "").lower()
    return hostname == "moddb.com" or hostname.endswith(".moddb.com")


@lru_cache(maxsize=256)
def resolve_moddb_url(url: str) -> str:
    """Resolve a ModDB page to the current mirror URL."""
    if not _is_moddb_url(url) or "/downloads/mirror/" in url:
        return url

    log("debug", f"Resolving ModDB mirror for {url}")
    try:
        from curl_cffi import requests
    except ImportError as exc:
        raise RuntimeError("ModDB downloads require the curl-cffi package") from exc

    response = requests.get(url, impersonate="chrome", timeout=30)
    if response.status_code != 200:
        raise RuntimeError(f"ModDB returned status {response.status_code} for {url}")

    if "/downloads/start/" not in response.url:
        match = re.search(r'href="[^"]*?/downloads/start/(\d+)"', response.text)
        if not match:
            raise RuntimeError(f"Could not find a ModDB download link on {url}")
        start_url = f"https://www.moddb.com/downloads/start/{match.group(1)}"
        response = requests.get(start_url, impersonate="chrome", timeout=30)
        if response.status_code != 200:
            raise RuntimeError(f"ModDB returned status {response.status_code} for the download link")

    match = re.search(r"(https://www\.moddb\.com/downloads/mirror/\d+/\d+/[a-f0-9]+)", response.text)
    if not match:
        raise RuntimeError(f"No ModDB mirror link found on {response.url}")
    log("debug", f"Resolved ModDB mirror: {match.group(1)}")
    return match.group(1)


@dataclass
class DownloadItem:
    url: str
    dest_path: Path
    gid: Optional[str] = None
    status: str = "pending"
    total_length: int = 0
    completed_length: int = 0
    download_speed: int = 0
    error_message: str = ""
    expected_hash: Optional[str] = None
    hash_retries: int = 0

    @property
    def progress_percent(self) -> float:
        if self.total_length <= 0:
            return 0.0
        return (self.completed_length / self.total_length) * 100


class Aria2RpcError(RuntimeError):
    pass


class Aria2Client:
    """Launch aria2c in RPC mode and talk to it over JSON-RPC."""

    def __init__(self, rpc_port: int = 6801, secret: Optional[str] = None):
        self.rpc_port = rpc_port
        self.secret = secret or secrets.token_hex(16)
        self.process: Optional[subprocess.Popen] = None
        self._job = None
        self._rpc_url = f"http://127.0.0.1:{self.rpc_port}/jsonrpc"
        self._request_id = 0

    def start(self):
        if not ARIA2C_PATH.exists():
            raise FileNotFoundError(f"aria2c executable not found at {ARIA2C_PATH}")
        if self.process is not None:
            return

        log("info", f"Starting aria2c on rpc port {self.rpc_port}")
        args = [
            str(ARIA2C_PATH),
            "--enable-rpc",
            f"--rpc-listen-port={self.rpc_port}",
            f"--rpc-secret={self.secret}",
            "--rpc-listen-all=false",
            "--quiet=true",
            "--allow-overwrite=true",
            "--auto-file-renaming=false",
            "--continue=true",
            "--max-concurrent-downloads=4",
            "--split=4",
            "--max-connection-per-server=4",
            "--min-split-size=10M",
            "--max-tries=0",
            "--retry-wait=10",
            "--connect-timeout=30",
            "--timeout=60",
            "--lowest-speed-limit=10K",
            "--check-integrity=true",
            "--auto-save-interval=10",
        ]
        creationflags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        self.process = subprocess.Popen(
            args,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=creationflags,
        )
        try:
            self._job = create_kill_on_close_job(self.process)
        except Exception:
            log("error", "Failed to attach aria2c to a job object, killing process")
            self.process.kill()
            self.process.wait()
            self.process = None
            raise
        self._wait_until_ready()
        log("info", "aria2c is ready")

    def _wait_until_ready(self, timeout: float = 10.0):
        deadline = time.time() + timeout
        last_error: Optional[Exception] = None
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("aria2c exited before becoming ready")
            try:
                self.call("aria2.getVersion")
                return
            except Exception as exc:
                last_error = exc
                time.sleep(0.1)
        raise TimeoutError(f"aria2c did not become ready in time: {last_error}")

    def call(self, method: str, params: Optional[list] = None):
        self._request_id += 1
        payload_params = [f"token:{self.secret}"]
        if params:
            payload_params.extend(params)

        payload = {
            "jsonrpc": "2.0",
            "id": str(self._request_id),
            "method": method,
            "params": payload_params,
        }
        request = urllib.request.Request(
            self._rpc_url,
            data=json.dumps(payload).encode("utf-8"),
            headers={"Content-Type": "application/json"},
        )

        try:
            with urllib.request.urlopen(request, timeout=5) as response:
                body = json.loads(response.read().decode("utf-8"))
        except urllib.error.URLError as exc:
            raise Aria2RpcError(f"failed to reach aria2c rpc: {exc}") from exc

        if "error" in body:
            raise Aria2RpcError(body["error"].get("message", "unknown aria2 error"))
        return body["result"]

    def add_uri(self, url: str, dest_dir: Path, filename: Optional[str] = None) -> str:
        options = {"dir": str(dest_dir)}
        if filename:
            options["out"] = filename
        return self.call("aria2.addUri", [[url], options])

    def tell_status(self, gid: str) -> dict:
        return self.call(
            "aria2.tellStatus",
            [gid, ["status", "totalLength", "completedLength", "downloadSpeed", "errorMessage"]],
        )

    def shutdown(self):
        if self.process is None:
            return
        log("info", "Shutting down aria2c")
        try:
            self.call("aria2.shutdown")
        except Exception:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            log("warning", "aria2c did not exit cleanly, killing it")
            self.process.kill()
        self.process = None
        close_job(self._job)
        self._job = None


class Downloader:
    """Coordinates a batch of downloads and reports progress."""

    def __init__(self, on_update: Callable[[List[DownloadItem]], None], poll_interval: float = 0.3):
        self.client = Aria2Client()
        self.on_update = on_update
        self.poll_interval = poll_interval
        self.items: List[DownloadItem] = []

    def add(self, url: str, dest_path: os.PathLike, expected_hash: Optional[str] = None):
        self.items.append(DownloadItem(url=url, dest_path=Path(dest_path), expected_hash=expected_hash))

    @staticmethod
    def _matches_hash(path: Path, expected_hash: Optional[str]) -> bool:
        if not expected_hash or not path.is_file():
            return False
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest() == expected_hash.lower()

    def _skip_existing_files(self):
        for item in self.items:
            if self._matches_hash(item.dest_path, item.expected_hash):
                log("info", f"{item.dest_path.name}: already present with a matching hash, skipping download")
                item.status = "complete"

    def _retry_after_hash_mismatch(self, item: DownloadItem):
        if item.hash_retries >= MAX_HASH_RETRIES:
            log("error", f"{item.dest_path.name}: hash validation failed after {MAX_HASH_RETRIES} retries")
            item.status = "error"
            item.error_message = "download failed hash validation after retries"
            return

        log("warning", f"{item.dest_path.name}: hash mismatch, retrying ({item.hash_retries + 1}/{MAX_HASH_RETRIES})")
        try:
            if item.dest_path.exists():
                item.dest_path.unlink()
            item.hash_retries += 1
            item.status = "resolving" if _is_moddb_url(item.url) else "waiting"
            item.error_message = (
                "Generating ModDB download link..."
                if item.status == "resolving"
                else "Retrying after hash mismatch..."
            )
            self.on_update(self.items)

            resolved_url = resolve_moddb_url(item.url)
            item.gid = self.client.add_uri(resolved_url, item.dest_path.parent, item.dest_path.name)
            item.total_length = 0
            item.completed_length = 0
            item.download_speed = 0
            item.status = "waiting"
            item.error_message = f"Hash mismatch - retrying ({item.hash_retries}/{MAX_HASH_RETRIES})"
        except Exception as exc:
            log("error", f"{item.dest_path.name}: could not requeue after hash mismatch: {exc}")
            item.status = "error"
            item.error_message = f"could not replace invalid download: {exc}"

    def run(self):
        """Launch aria2, queue each item, and poll until all downloads finish."""
        self._skip_existing_files()
        if self._all_done():
            log("info", "All files already present with valid hashes, nothing to download")
            self.on_update(self.items)
            return

        log("info", f"Starting download batch: {len(self.items)} item(s)")
        try:
            self.client.start()
            for item in self.items:
                if item.status == "complete":
                    continue
                item.dest_path.parent.mkdir(parents=True, exist_ok=True)
                if _is_moddb_url(item.url):
                    item.status = "resolving"
                    self.on_update(self.items)
                resolved_url = resolve_moddb_url(item.url)
                item.gid = self.client.add_uri(resolved_url, item.dest_path.parent, item.dest_path.name)
                item.status = "waiting"
                log("debug", f"{item.dest_path.name}: queued (gid={item.gid})")

            while not self._all_done():
                self._poll_once()
                self.on_update(self.items)
                time.sleep(self.poll_interval)
            self._poll_once()
            self.on_update(self.items)
            log("info", "Download batch finished")
        finally:
            self.client.shutdown()

    def _poll_once(self):
        for item in self.items:
            if item.status in ("complete", "error"):
                continue
            try:
                status = self.client.tell_status(item.gid)
            except Exception as exc:
                log("error", f"{item.dest_path.name}: lost contact with aria2c: {exc}")
                item.status = "error"
                item.error_message = str(exc)
                continue

            item.total_length = int(status.get("totalLength", 0))
            item.completed_length = int(status.get("completedLength", 0))
            item.download_speed = int(status.get("downloadSpeed", 0))
            aria_status = status.get("status", "active")

            if aria_status == "complete":
                if item.expected_hash:
                    item.status = "validating"
                    item.error_message = "Verifying SHA-256..."
                    self.on_update(self.items)
                if item.expected_hash and not self._matches_hash(item.dest_path, item.expected_hash):
                    self._retry_after_hash_mismatch(item)
                else:
                    log("info", f"{item.dest_path.name}: download complete")
                    item.status = "complete"
            elif aria_status == "error":
                log("error", f"{item.dest_path.name}: {status.get('errorMessage', 'download failed')}")
                item.status = "error"
                item.error_message = status.get("errorMessage", "download failed")
            elif aria_status in {"active", "waiting", "paused"}:
                item.status = aria_status
            else:
                item.status = "active"

    def _all_done(self) -> bool:
        return all(item.status in {"complete", "error"} for item in self.items)


def format_speed(bytes_per_sec: int) -> str:
    if bytes_per_sec >= 1024 * 1024:
        return f"{bytes_per_sec / (1024 * 1024):.1f} MB/s"
    if bytes_per_sec >= 1024:
        return f"{bytes_per_sec / 1024:.1f} KB/s"
    return f"{bytes_per_sec} B/s"


def download_all(
    downloads: Iterable[tuple],
    on_update: Callable[[List[DownloadItem]], None],
    poll_interval: float = 0.3,
) -> List[DownloadItem]:
    """Download each (url, dest_path[, hash]) tuple and return the item list."""
    downloader = Downloader(on_update=on_update, poll_interval=poll_interval)
    for download in downloads:
        url, dest_path = download[:2]
        expected_hash = download[2] if len(download) > 2 else None
        downloader.add(url, dest_path, expected_hash)
    downloader.run()
    failed = [item.dest_path.name for item in downloader.items if item.status != "complete"]
    if failed:
        log("warning", f"download_all finished with failures: {failed}")
    else:
        log("info", f"download_all finished, all {len(downloader.items)} item(s) complete")
    return downloader.items
