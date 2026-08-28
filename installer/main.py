import ctypes
import queue
import re
import subprocess
import sys
import threading
import time
import tkinter as tk
from pathlib import Path
from tkinter import messagebox, ttk

import yaml

import downloader
import installer
from log_custom import log
from relocate import check_and_relocate, handle_deletion_argument

MULTIPART_RE = re.compile(r"^(.*)\.(\d{3,})$")

CONFIG_PATH = Path(__file__).parent / "config.yaml"
DOWNLOADS_DIR = Path(__file__).parent / "downloads"
ANOMALY_EXE = Path(__file__).parent / ".." / "anomaly" / "bin" / "AnomalyDX11AVX.exe"


def load_config() -> dict:
    if not CONFIG_PATH.exists():
        raise FileNotFoundError(f"config.yaml not found at {CONFIG_PATH}")
    log("debug", f"Loading config from {CONFIG_PATH}")
    with CONFIG_PATH.open("r", encoding="utf-8") as handle:
        return yaml.safe_load(handle)


def get_downloads_for_selection(assets, selected_ids):
    """Return (url, destination, expected_hash) tuples for base assets plus any asset
    tied to a choice the user selected."""
    selected = set(selected_ids)
    entries = []
    for asset in assets:
        if asset["choice"] == "base" or asset["choice"] in selected:
            entries.append((asset["url"], DOWNLOADS_DIR / asset["file"], asset.get("sha256")))
    log("debug", f"Resolved manifest for selection {sorted(selected)}: {len(entries)} file(s)")
    return entries


def get_steps_for_selection(steps, selected_ids):
    """Return steps filtered to include base steps and steps tied to selected choices.
    Steps without a 'choice' field always run. Steps with a 'choice' field only run
    if that choice is in selected_ids."""
    selected = set(selected_ids)
    filtered = []
    for step in steps:
        if "choice" not in step or step["choice"] in selected:
            filtered.append(step)
    log("debug", f"Resolved steps for selection {sorted(selected)}: {len(filtered)} step(s)")
    return filtered


def group_manifest(manifest):
    """Group numbered archive parts (e.g. .001, .002) sharing a common base name."""
    groups = {}
    order = []
    for _, dest, expected_hash in manifest:
        match = MULTIPART_RE.match(dest.name)
        key = match.group(1) if match else dest.name
        if key not in groups:
            groups[key] = []
            order.append(key)
        groups[key].append((dest.name, expected_hash))

    result = []
    for key in order:
        members = groups[key]
        if len(members) > 1:
            names = sorted(name for name, _ in members)
            nums = [MULTIPART_RE.match(name).group(2) for name in names]
            label = f"{key}.{nums[0]}-{nums[-1]}"
            result.append({"type": "group", "label": label, "members": members})
        else:
            name, expected_hash = members[0]
            result.append({"type": "single", "name": name, "expected_hash": expected_hash})
    return result


def describe_item(item):
    """Return (percent, text, is_error) describing a single file's progress payload."""
    status = item["status"]
    if status == "complete":
        message = "Done - SHA-256 verified" if item.get("hash_valid") else "Done - hash not configured"
        return 100, message, False
    if status == "error":
        return item["percent"], "Error: " + item["error"], True
    if status == "waiting":
        return item["percent"], item.get("error") or f"Queued - {item['percent']:.0f}%", False
    if status == "paused":
        return item["percent"], f"Paused - {item['percent']:.0f}%", False
    if status == "resolving":
        return item["percent"], "Generating ModDB download link...", False
    if status == "validating":
        return item["percent"], "Verifying SHA-256...", False
    speed = downloader.format_speed(item["speed"])
    activity = "Downloading" if item["speed"] else "Connecting or retrying"
    return item["percent"], f"{activity} - {item['percent']:.0f}% - {speed}", False


CONFIG = load_config()
if CONFIG.get("debug_console", False):
    import log_custom
    log_custom.configure_console_logging(True)

if "show_7z_console" in CONFIG:
    installer.set_show_7z_console(bool(CONFIG["show_7z_console"]))

if CONFIG.get("debug_console", False) and CONFIG.get("show_7z_console", False):
    log("warning", "debug_console and show_7z_console are both enabled; 7z output is shown in its own console window and will not be captured for live install progress parsing.")

CHOICES = CONFIG["choices"]
ASSETS = CONFIG["assets"]
STEPS = CONFIG["steps"]
log("info", f"Config loaded: {len(CHOICES)} top-level choice(s), {len(ASSETS)} asset(s), {len(STEPS)} step(s)")


class ScrollableFrame(ttk.Frame):
    """A frame with a vertical scrollbar that keeps content scrollable."""

    def __init__(self, master, **kwargs):
        super().__init__(master, **kwargs)
        background = ttk.Style(self).lookup("TFrame", "background")
        self.canvas = tk.Canvas(self, background=background, highlightthickness=0)
        self.scrollbar = ttk.Scrollbar(self, orient="vertical", command=self.canvas.yview)
        self.canvas.configure(yscrollcommand=self.scrollbar.set)
        self.canvas.pack(side="left", fill="both", expand=True)
        self.scrollbar.pack(side="right", fill="y")
        self._scrollable = False

        self.inner = ttk.Frame(self.canvas, style="TFrame")
        self._inner_window = self.canvas.create_window((0, 0), window=self.inner, anchor="nw")
        self.inner.bind("<Configure>", self._update_scroll_state)
        self.canvas.bind("<Configure>", self._on_canvas_configure)
        self.canvas.bind("<Enter>", lambda _event: self.canvas.bind_all("<MouseWheel>", self._on_mousewheel))
        self.canvas.bind("<Leave>", lambda _event: self.canvas.unbind_all("<MouseWheel>"))

    def _update_scroll_state(self, _event=None):
        self.canvas.configure(scrollregion=self.canvas.bbox("all"))
        self.after_idle(self._set_scrollable)

    def _on_canvas_configure(self, event):
        self.canvas.itemconfig(self._inner_window, width=event.width)
        self._update_scroll_state()

    def _set_scrollable(self):
        bounds = self.canvas.bbox("all")
        content_height = bounds[3] - bounds[1] if bounds else 0
        self._scrollable = content_height > self.canvas.winfo_height()
        if self._scrollable:
            self.scrollbar.pack(side="right", fill="y")
        else:
            self.scrollbar.pack_forget()
            self.canvas.yview_moveto(0)

    def _on_mousewheel(self, event):
        if not self._scrollable:
            return
        self.canvas.yview_scroll(int(-1 * (event.delta / 120)), "units")


class SelectionFrame(ttk.Frame):
    """Guides the user through the nested download tree until a leaf is chosen."""

    def __init__(self, master, on_complete):
        super().__init__(master, padding=20)
        self.on_complete = on_complete
        self.node_stack = [CHOICES]
        self.crumbs = []
        self.selected_ids = []
        self.choice_var = tk.StringVar()
        self.choice_var.trace_add("write", self._on_choice_changed)
        self._render_generation = 0

        self.title_label = ttk.Label(self, text="Select an option", style="Heading.TLabel")
        self.title_label.pack(anchor="w", pady=(0, 10))

        self.options_scroll = ScrollableFrame(self)
        self.options_scroll.pack(fill="both", expand=True, pady=(0, 20))

        button_row = ttk.Frame(self)
        button_row.pack(fill="x")
        self.back_btn = ttk.Button(button_row, text="Back", command=self._go_back)
        self.back_btn.pack(side="left")
        self.notice_label = ttk.Label(button_row, text="Powered by Gtair's Modular Installer for S.T.A.L.K.E.R. Zona", style="Muted.TLabel")
        self.notice_label.pack(side="left")
        self.next_btn = ttk.Button(button_row, text="Next", command=self._go_next, state="disabled")
        self.next_btn.pack(side="right")

        self._render_step()

    def _render_step(self):
        self._render_generation += 1
        render_id = self._render_generation
        for child in self.options_scroll.inner.winfo_children():
            child.destroy()
        self.options_scroll.canvas.yview_moveto(0)

        title = " > ".join(["Zona Installer", *self.crumbs]) if self.crumbs else "Select an option"
        self.title_label.config(text=title)
        self.choice_var.set("")

        options = self.node_stack[-1]
        for choice in options:
            option_frame = ttk.Frame(self.options_scroll.inner)
            option_frame.pack(fill="x", pady=2)
            ttk.Radiobutton(
                option_frame, text=choice["label"], value=choice["label"], variable=self.choice_var
            ).pack(anchor="w")
            description = choice.get("description", "")
            if description:
                ttk.Label(option_frame, text=description, style="Muted.TLabel").pack(anchor="w", padx=(20, 0))

        if self.crumbs:
            self.back_btn.pack(side="left")
            self.notice_label.pack_forget()
        else:
            self.back_btn.pack_forget()
            self.notice_label.pack(side="left")
        self.next_btn.config(state="disabled")

        if len(options) == 1:
            self.after_idle(lambda: render_id == self._render_generation and self._select_option(options[0]))

    def _on_choice_changed(self, *_):
        self.next_btn.config(state="normal" if self.choice_var.get() else "disabled")

    def _go_back(self):
        if not self.crumbs:
            return
        log("debug", f"User went back from {self.crumbs}")
        self.crumbs.pop()
        self.selected_ids.pop()
        self.node_stack.pop()
        self._render_step()

    def _go_next(self):
        label = self.choice_var.get()
        if not label:
            return
        choice = next(c for c in self.node_stack[-1] if c["label"] == label)
        self._select_option(choice)

    def _select_option(self, choice):
        self.crumbs.append(choice["label"])
        self.selected_ids.append(choice["id"])
        log("debug", f"User selected {choice['id']!r} ({choice['label']!r})")
        children = choice.get("choices") or []
        if not children:
            log("info", f"Selection complete: {self.crumbs} -> ids={self.selected_ids}")
            self.on_complete(list(self.crumbs), list(self.selected_ids))
            return
        self.node_stack.append(children)
        self._render_step()


class FileProgressRow(ttk.Frame):
    """A single file row showing status, hash, and progress."""

    def __init__(self, master, name, expected_hash):
        super().__init__(master)
        ttk.Label(self, text=name).pack(anchor="w")
        hash_text = expected_hash or "not configured"
        ttk.Label(self, text=f"SHA-256: {hash_text}", style="MutedSmall.TLabel").pack(anchor="w")
        self.progress = ttk.Progressbar(self, orient="horizontal", length=350, mode="determinate")
        self.progress.pack(fill="x", pady=(2, 0))
        self.status_label = ttk.Label(self, text="Preparing download...", style="Muted.TLabel")
        self.status_label.pack(anchor="w")

    def update_status(self, percent, status_text):
        self.progress["value"] = percent
        self.status_label.config(text=status_text)

    def mark_error(self, status_text):
        self.progress.configure(style="Error.Horizontal.TProgressbar")
        self.progress["value"] = 100
        self.status_label.config(text=status_text)


class GroupProgressRow(ttk.Frame):
    """A combined progress bar for numbered archive parts, with per-part detail lines."""

    def __init__(self, master, label, member_names):
        super().__init__(master)
        ttk.Label(self, text=label).pack(anchor="w")
        self.progress = ttk.Progressbar(self, orient="horizontal", length=350, mode="determinate")
        self.progress.pack(fill="x", pady=(2, 0))
        self.member_labels = {}
        self.percents = {}
        for name in member_names:
            self.percents[name] = 0.0
            label_widget = ttk.Label(self, text=f"{name}: Preparing download...", style="MutedSmall.TLabel")
            label_widget.pack(anchor="w")
            self.member_labels[name] = label_widget

    def update_member(self, name, percent, text, is_error):
        self.percents[name] = percent
        self.member_labels[name].config(text=f"{name}: {text}")
        if is_error:
            self.progress.configure(style="Error.Horizontal.TProgressbar")
        self.progress["value"] = sum(self.percents.values()) / len(self.percents)

    def mark_error(self, status_text):
        self.progress.configure(style="Error.Horizontal.TProgressbar")
        self.progress["value"] = 100


class ProgressFrame(ttk.Frame):
    def __init__(self, master, selection, manifest, on_cancel, on_continue):
        super().__init__(master, padding=20)
        self.on_cancel = on_cancel
        self.on_continue = on_continue
        self._complete = False

        ttk.Label(self, text="Downloading: " + " / ".join(selection), style="Heading.TLabel").pack(
            anchor="w", pady=(0, 15)
        )

        self.rows_scroll = ScrollableFrame(self)
        self.rows_scroll.pack(fill="both", expand=True)

        self.total_files = len(manifest)
        self.all_rows = []
        self.single_rows = {}
        self.group_rows = {}
        self.member_to_group = {}

        grouped = group_manifest(manifest)
        for index, entry in enumerate(grouped):
            if entry["type"] == "group":
                names = sorted(name for name, _ in entry["members"])
                row = GroupProgressRow(self.rows_scroll.inner, entry["label"], names)
                self.group_rows[entry["label"]] = row
                for name in names:
                    self.member_to_group[name] = entry["label"]
            else:
                row = FileProgressRow(self.rows_scroll.inner, entry["name"], entry["expected_hash"])
                self.single_rows[entry["name"]] = row

            self.all_rows.append(row)
            row.pack(fill="x", pady=(0, 10))
            if index < len(grouped) - 1:
                ttk.Separator(self.rows_scroll.inner, orient="horizontal").pack(fill="x", pady=(0, 10))

        bottom_row = ttk.Frame(self)
        bottom_row.pack(fill="x", pady=(10, 0))
        self.overall_label = ttk.Label(bottom_row, text="Starting...")
        self.overall_label.pack(side="left")
        self.action_btn = ttk.Button(bottom_row, text="Cancel", command=self._on_action_clicked)
        self.action_btn.pack(side="right")

    def _on_action_clicked(self):
        if self._complete:
            self.on_continue()
        else:
            self.on_cancel()

    def update_progress(self, items):
        done = 0
        for item in items:
            name = item["name"]
            percent, text, is_error = describe_item(item)
            if item["status"] == "complete":
                done += 1

            if name in self.single_rows:
                self.single_rows[name].update_status(percent, text)
                if is_error:
                    self.single_rows[name].mark_error(text)
            elif name in self.member_to_group:
                self.group_rows[self.member_to_group[name]].update_member(name, percent, text, is_error)

        self.overall_label.config(text=f"{done}/{self.total_files} files complete")

    def mark_done(self):
        self.overall_label.config(text="Download complete!")
        self._complete = True
        self.action_btn.config(text="Continue")

    def mark_failed(self, message):
        for row in self.all_rows:
            if row.progress["value"] < 100:
                row.mark_error("Error: " + message)
        self.overall_label.config(text="Download failed")
        self.action_btn.config(state="disabled")


def _step_description(step):
    """Human-readable description of what a step is actually doing."""
    action = step["action"]
    if action == "create_dir":
        return "Preparing folders"
    if action == "verify_game":
        return "Waiting for you to verify the game"
    if action == "extract":
        if "file" in step:
            return f"Extracting {Path(step['file']).name}"
        return "Installing selected settings"
    return step["name"].replace("_", " ").capitalize()


class InstallFrame(ttk.Frame):
    """One combined progress bar covering every install step, with the current job's
    description and (when 7z.exe reports it) its live extraction percentage underneath."""

    def __init__(self, master, steps):
        super().__init__(master, padding=20)
        self.total_steps = len(steps)
        self.completed_steps = 0

        ttk.Label(self, text="Installing...", style="Heading.TLabel").pack(anchor="w", pady=(0, 15))

        self.progress = ttk.Progressbar(self, orient="horizontal", length=350, mode="determinate")
        self.progress.pack(fill="x", pady=(0, 10))

        self.job_label = ttk.Label(self, text="Preparing...")
        self.job_label.pack(anchor="w")
        self.detail_label = ttk.Label(self, text="", style="Muted.TLabel")
        self.detail_label.pack(anchor="w")

    def _set_bar(self, fraction_within_current_step):
        overall = (self.completed_steps + fraction_within_current_step) / self.total_steps * 100
        self.progress["value"] = min(overall, 100)

    def set_step_running(self, step):
        self.job_label.config(text=f"Job {self.completed_steps + 1}/{self.total_steps}: {_step_description(step)}")
        if step.get("action") == "extract":
            if "file" in step:
                self.detail_label.config(text=f"Extracting {Path(step['file']).name}...")
            else:
                self.detail_label.config(text="Installing selected files...")
        else:
            self.detail_label.config(text="Working...")
        self._set_bar(0)

    def set_step_progress(self, percent, detail=None):
        if detail:
            self.detail_label.config(text=f"{percent}% Extracted - {detail}")
        else:
            self.detail_label.config(text=f"{percent}% Extracted")
        self._set_bar(percent / 100)

    def set_step_done(self):
        self.completed_steps += 1
        self._set_bar(0)

    def mark_done(self):
        self.job_label.config(text="Installation complete!")
        self.detail_label.config(text="")
        self.progress["value"] = 100

    def mark_failed(self, message):
        self.progress.configure(style="Error.Horizontal.TProgressbar")
        self.job_label.config(text="Installation failed")
        self.detail_label.config(text=message, foreground="#d9534f")


class VerifyGameDialog(tk.Toplevel):
    """Modal step: launch Anomaly directly so the user can confirm the modded exe works."""

    def __init__(self, master, on_continue):
        super().__init__(master)
        self.on_continue = on_continue

        self.title("Verify installation")
        self.resizable(False, False)
        self.protocol("WM_DELETE_WINDOW", lambda: None)  # must go through Continue
        self.transient(master)
        self.grab_set()

        frame = ttk.Frame(self, padding=20)
        frame.pack(fill="both", expand=True)

        ttk.Label(frame, text="Verify the game", style="Heading.TLabel").pack(anchor="w", pady=(0, 10))
        message = (
            "Launch a test run of Anomaly and confirm the modded executable works.\n"
            "Close the game once you've checked it, then press Continue."
        )
        ttk.Label(frame, text=message, style="Muted.TLabel", wraplength=360, justify="left").pack(
            anchor="w", pady=(0, 15)
        )

        self.launch_btn = ttk.Button(frame, text="Launch Test", command=self._launch)
        self.launch_btn.pack(anchor="w")

        self.continue_btn = ttk.Button(frame, text="Continue", command=self._continue, state="disabled")

    def _launch(self):
        self.launch_btn.pack_forget()
        self.continue_btn.pack(anchor="w")
        log("info", f"Launching {ANOMALY_EXE} for verification")
        process = subprocess.Popen([str(ANOMALY_EXE)], cwd=ANOMALY_EXE.parent)
        threading.Thread(target=self._wait_for_exit, args=(process,), daemon=True).start()

    def _wait_for_exit(self, process):
        process.wait()
        exit_code = process.returncode
        log("info", f"Anomaly exited with code {exit_code}")

        if exit_code != 0:
            self.after(
                0,
                lambda: (
                    messagebox.showerror(
                        "Verification failed",
                        f"Anomaly exited with code {exit_code}. The game did not start cleanly, so installation cannot continue.",
                        parent=self,
                    ),
                    self.grab_release(),
                    self.destroy(),
                    self.master.destroy(),
                ),
            )
            return

        self.after(0, lambda: self.continue_btn.config(state="normal"))

    def _continue(self):
        log("info", "User confirmed verification, resuming install")
        self.grab_release()
        self.destroy()
        self.on_continue()


class DownloaderApp:
    def __init__(self, root):
        self.root = root
        self.progress_queue = queue.Queue()
        self.selected_ids = []
        self.ctx = {}
        self.verify_step = None
        self.steps_before_verify = []
        self.steps_after_verify = []

        style = ttk.Style(root)
        style.theme_use("clam")
        style.configure("Heading.TLabel", font=("Segoe UI", 14, "bold"))
        style.configure("Muted.TLabel", foreground="#666666")
        style.configure("MutedSmall.TLabel", foreground="#777777", font=("Segoe UI", 7))
        style.configure("Error.Horizontal.TProgressbar", background="#d9534f", troughcolor="#f5c6cb")

        self.root.title("Zona Installer")
        self.root.geometry("500x400")
        self.root.resizable(False, False)

        self.current_frame = SelectionFrame(self.root, self._on_selection_complete)
        self.current_frame.pack(fill="both", expand=True)
        self._poll_progress_queue()

    def _on_selection_complete(self, labels, selected_ids):
        self.selected_ids = selected_ids
        manifest = get_downloads_for_selection(ASSETS, selected_ids)
        log("info", f"Starting download phase: {len(manifest)} file(s) for selection {labels}")
        self.current_frame.destroy()
        self.current_frame = ProgressFrame(
            self.root, labels, manifest, on_cancel=self._on_download_cancel, on_continue=self._start_install
        )
        self.current_frame.pack(fill="both", expand=True)
        threading.Thread(target=run_download, args=(manifest, self.progress_queue), daemon=True).start()

    def _on_download_cancel(self):
        confirmed = messagebox.askyesno(
            "Cancel download?",
            "Cancel the download? Your progress is saved - re-run the installer and it will "
            "pick up right where you left off.",
            parent=self.root,
        )
        if not confirmed:
            return
        log("info", "User cancelled the download")
        self.root.destroy()

    def _start_install(self):
        filtered_steps = get_steps_for_selection(STEPS, self.selected_ids)
        verify_index = next(i for i, step in enumerate(filtered_steps) if step["action"] == "verify_game")
        self.verify_step = filtered_steps[verify_index]
        self.steps_before_verify = filtered_steps[:verify_index]
        self.steps_after_verify = filtered_steps[verify_index + 1 :]
        self.ctx = {"assets": ASSETS, "selected_choice_ids": set(self.selected_ids)}

        log("info", "Download phase complete, starting install phase")
        self.current_frame.destroy()
        self.current_frame = InstallFrame(self.root, filtered_steps)
        self.current_frame.pack(fill="both", expand=True)
        self._run_step_phase(self.steps_before_verify, self._show_verify_dialog)

    def _run_step_phase(self, steps, on_phase_done):
        def worker():
            try:
                installer.run_steps(
                    steps,
                    self.ctx,
                    on_step_start=lambda step: self.progress_queue.put(("step_start", step)),
                    on_step_progress=lambda step, percent, detail=None: self.progress_queue.put(("step_progress", percent, detail)),
                    on_step_done=lambda step: self.progress_queue.put(("step_done",)),
                )
                self.progress_queue.put(("phase_done", on_phase_done))
            except Exception as exc:
                log("error", f"Install step phase failed: {exc}")
                self.progress_queue.put(("install_error", str(exc)))

        threading.Thread(target=worker, daemon=True).start()

    def _show_verify_dialog(self):
        log("info", "Reached verify_game step, showing dialog")
        if isinstance(self.current_frame, InstallFrame):
            self.current_frame.set_step_running(self.verify_step)
        VerifyGameDialog(self.root, on_continue=self._on_verify_continue)

    def _on_verify_continue(self):
        if isinstance(self.current_frame, InstallFrame):
            self.current_frame.set_step_done()
        self._run_step_phase(self.steps_after_verify, self._finish_install)

    def _finish_install(self):
        log("info", "Installation complete")
        if isinstance(self.current_frame, InstallFrame):
            self.current_frame.mark_done()

    def _poll_progress_queue(self):
        try:
            while True:
                kind, *payload = self.progress_queue.get_nowait()
                if kind == "progress":
                    (items,) = payload
                    if isinstance(self.current_frame, ProgressFrame):
                        self.current_frame.update_progress(items)
                elif kind == "done":
                    if isinstance(self.current_frame, ProgressFrame):
                        self.current_frame.mark_done()
                elif kind == "error":
                    message, *details = payload
                    log("error", f"Download failed: {message}")
                    if isinstance(self.current_frame, ProgressFrame):
                        if details and details[0]:
                            self.current_frame.mark_failed(message)
                        else:
                            self.current_frame.overall_label.config(text="Failed: " + message)
                    messagebox.showerror("Download failed", message, parent=self.root)
                    self.root.destroy()
                    return
                elif kind == "step_start":
                    (step,) = payload
                    if isinstance(self.current_frame, InstallFrame):
                        self.current_frame.set_step_running(step)
                elif kind == "step_progress":
                    (percent, detail) = payload
                    if isinstance(self.current_frame, InstallFrame):
                        self.current_frame.set_step_progress(percent, detail)
                elif kind == "step_done":
                    if isinstance(self.current_frame, InstallFrame):
                        self.current_frame.set_step_done()
                elif kind == "phase_done":
                    (on_phase_done,) = payload
                    on_phase_done()
                elif kind == "install_error":
                    (message,) = payload
                    log("error", f"Installation failed: {message}")
                    if isinstance(self.current_frame, InstallFrame):
                        self.current_frame.mark_failed(message)
                    messagebox.showerror("Installation failed", message, parent=self.root)
                    self.root.destroy()
                    return
        except queue.Empty:
            pass
        self.root.after(100, self._poll_progress_queue)


def run_gui():
    log("info", "Launching GUI")
    root = tk.Tk()
    DownloaderApp(root)
    root.mainloop()
    log("info", "GUI closed")


def run_download(manifest, progress_queue):
    """Download every item in the manifest and emit progress updates."""

    def on_update(items):
        payload = [
            {
                "name": item.dest_path.name,
                "percent": item.progress_percent,
                "speed": item.download_speed,
                "status": item.status,
                "error": item.error_message,
                "hash_valid": item.status == "complete" and bool(item.expected_hash),
            }
            for item in items
        ]
        progress_queue.put(("progress", payload))

    try:
        items = downloader.download_all(manifest, on_update)
        failed = [
            f"{item.dest_path.name}: {item.error_message or item.status}"
            for item in items
            if item.status != "complete"
        ]
        if failed:
            progress_queue.put(("error", "; ".join(failed)))
            return
        progress_queue.put(("done",))
    except Exception as exc:
        log("error", f"Download phase raised an exception: {exc}")
        progress_queue.put(("error", str(exc), True))


def main():
    mutex_name = "GTAIR-MODULAR-INSTALLER"
    kernel32 = ctypes.windll.kernel32

    if len(sys.argv) > 1 and sys.argv[1] == "/delete":
        time.sleep(1)
    # Create the mutex first
    mutex = kernel32.CreateMutexW(None, False, mutex_name)
    # If mutex already exists, wait for the other instance to exit
    if kernel32.GetLastError() == 183:  # ERROR_ALREADY_EXISTS
        log("error", "Another instance is running.")
        sys.exit(1)
    
    # Now handle /delete argument (cleanup of old installation directory)
    if len(sys.argv) > 1 and sys.argv[1] == "/delete":
        if len(sys.argv) > 2:
            folder_to_delete = sys.argv[2]
            log("info", f"Deletion mode: {folder_to_delete}")
            handle_deletion_argument(folder_to_delete)
            # Continue as normal after deletion
        else:
            log("error", "Deletion mode requires a folder path argument")
            sys.exit(1)
    
    # Check if installer is in correct location, relocate if needed
    should_continue, install_dir = check_and_relocate()
    if not should_continue:
        log("info", "Relocation required, exiting")
        sys.exit()
    
    log("info", "Zona Installer starting")
    run_gui()


if __name__ == "__main__":
    main()
