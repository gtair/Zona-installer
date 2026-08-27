import configparser
import queue
import re
import threading
import tkinter as tk
from pathlib import Path
from tkinter import messagebox, ttk

import downloader

MULTIPART_RE = re.compile(r"^(.*)\.(\d{3,})$")

CONFIG_PATH = Path(__file__).parent / "config.ini"
DOWNLOADS_DIR = Path(__file__).parent / "downloads"
BASE_SECTION = "base"
DOWNLOADS_PREFIX = "downloads:"


def load_config() -> configparser.ConfigParser:
    parser = configparser.ConfigParser()
    parser.optionxform = str
    if not CONFIG_PATH.exists():
        raise FileNotFoundError(f"config.ini not found at {CONFIG_PATH}")
    parser.read(CONFIG_PATH, encoding="utf-8")
    return parser


def build_options(parser):
    """Build the nested option tree from configured download sections."""
    options = {}
    for section in parser.sections():
        if not section.startswith(DOWNLOADS_PREFIX):
            continue

        parts = [part.strip() for part in section[len(DOWNLOADS_PREFIX) :].split("/") if part.strip()]
        node = options
        for part in parts[:-1]:
            node = node.setdefault(part, {})
        if parts:
            node.setdefault(parts[-1], None)
    return options


def get_downloads_for_selection(parser, selection):
    """Return (url, destination, expected_hash) tuples for the selected path."""
    entries = []
    seen_names = set()

    def add_section(section_name: str):
        if not parser.has_section(section_name):
            return
        for name, url in parser.items(section_name):
            if name in seen_names:
                continue
            seen_names.add(name)
            expected_hash = parser.get("hashes", name, fallback="").strip().lower() or None
            entries.append((url, DOWNLOADS_DIR / name, expected_hash))

    add_section(BASE_SECTION)
    prefix_parts = []
    for part in selection:
        prefix_parts.append(part)
        add_section(DOWNLOADS_PREFIX + "/".join(prefix_parts))
    return entries


def get_description(parser, path_parts):
    """Return the optional description for the selected path."""
    return parser.get("descriptions", "/".join(path_parts), fallback="")


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
OPTIONS = build_options(CONFIG)


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
        self.path = []
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
        self.next_btn = ttk.Button(button_row, text="Next", command=self._go_next, state="disabled")
        self.next_btn.pack(side="right")

        self._render_step()

    def _render_step(self):
        self._render_generation += 1
        render_id = self._render_generation
        for child in self.options_scroll.inner.winfo_children():
            child.destroy()
        self.options_scroll.canvas.yview_moveto(0)

        title = " > ".join(["Zona Downloader", *self.path]) if self.path else "Select an option"
        self.title_label.config(text=title)
        self.choice_var.set("")

        for label in self._current_node():
            option_frame = ttk.Frame(self.options_scroll.inner)
            option_frame.pack(fill="x", pady=2)
            ttk.Radiobutton(option_frame, text=label, value=label, variable=self.choice_var).pack(anchor="w")
            description = get_description(CONFIG, self.path + [label])
            if description:
                ttk.Label(option_frame, text=description, style="Muted.TLabel").pack(anchor="w", padx=(20, 0))

        self.back_btn.config(state="normal" if self.path else "disabled")
        self.next_btn.config(state="disabled")

        node = self._current_node()
        if len(node) == 1:
            self.after_idle(lambda: render_id == self._render_generation and self._select_option(next(iter(node))))

    def _on_choice_changed(self, *_):
        self.next_btn.config(state="normal" if self.choice_var.get() else "disabled")

    def _current_node(self):
        node = OPTIONS
        for part in self.path:
            node = node[part]
        return node

    def _go_back(self):
        if not self.path:
            return
        self.path.pop()
        self._render_step()

    def _go_next(self):
        choice = self.choice_var.get()
        if not choice:
            return
        self._select_option(choice)

    def _select_option(self, choice):
        child = self._current_node()[choice]
        self.path.append(choice)
        if child is None:
            self.on_complete(list(self.path))
            return
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
    def __init__(self, master, selection, manifest):
        super().__init__(master, padding=20)
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

        self.overall_label = ttk.Label(self, text="Starting...")
        self.overall_label.pack(anchor="w")

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

    def mark_failed(self, message):
        for row in self.all_rows:
            if row.progress["value"] < 100:
                row.mark_error("Error: " + message)
        self.overall_label.config(text="Download failed")


class DownloaderApp:
    def __init__(self, root):
        self.root = root
        self.progress_queue = queue.Queue()

        style = ttk.Style(root)
        style.theme_use("clam")
        style.configure("Heading.TLabel", font=("Segoe UI", 14, "bold"))
        style.configure("Muted.TLabel", foreground="#666666")
        style.configure("MutedSmall.TLabel", foreground="#777777", font=("Segoe UI", 7))
        style.configure("Error.Horizontal.TProgressbar", background="#d9534f", troughcolor="#f5c6cb")

        self.root.title("Zona Downloader")
        self.root.geometry("500x400")
        self.root.resizable(False, False)

        self.current_frame = SelectionFrame(self.root, self._on_selection_complete)
        self.current_frame.pack(fill="both", expand=True)
        self._poll_progress_queue()

    def _on_selection_complete(self, selection):
        manifest = get_downloads_for_selection(CONFIG, selection)
        self.current_frame.destroy()
        self.current_frame = ProgressFrame(self.root, selection, manifest)
        self.current_frame.pack(fill="both", expand=True)
        threading.Thread(target=run_download, args=(manifest, self.progress_queue), daemon=True).start()

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
                    if isinstance(self.current_frame, ProgressFrame):
                        if details and details[0]:
                            self.current_frame.mark_failed(message)
                        else:
                            self.current_frame.overall_label.config(text="Failed: " + message)
                    messagebox.showerror("Download failed", message, parent=self.root)
                    self.root.destroy()
                    return
        except queue.Empty:
            pass
        self.root.after(100, self._poll_progress_queue)


def run_gui():
    root = tk.Tk()
    DownloaderApp(root)
    root.mainloop()


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
        progress_queue.put(("error", str(exc), True))


def main():
    run_gui()


if __name__ == "__main__":
    main()
