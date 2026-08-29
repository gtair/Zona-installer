import logging
import datetime
import sys
import inspect
import os
import io
import ctypes
import traceback
import threading
from pathlib import Path

# 1. Setup the filename once when this module is first imported
LOG_DIR = Path(__file__).parent / "logs"
LOG_DIR.mkdir(exist_ok=True)

_start_time = datetime.datetime.now().strftime("%Y-%m-%d_%H-%M-%S")
_log_file = LOG_DIR / f"log_{_start_time}.log"

# 2. Define the look with fixed-width source columns
class CustomFormatter(logging.Formatter):
    def format(self, record):
        # Support for the custom log() function's extra metadata
        if hasattr(record, 'caller_filename'):
            record.filename = record.caller_filename
            record.funcName = record.caller_funcName

        if hasattr(record, 'source_override'):
            record.source = record.source_override
        else:
            # Create the source string (file:function)
            combined_source = f"{record.filename}:{record.funcName}"

            # Robust alignment: Truncate with an ellipsis if > 30, pad if < 30
            if len(combined_source) > 30:
                record.source = combined_source[:27] + "..."
            else:
                record.source = combined_source.ljust(30)

        return super().format(record)

# The -8s pads the level name (DEBUG, INFO, etc.) so the brackets stay aligned
log_format_str = "%(asctime)s.%(msecs)03d [%(levelname)-8s] %(source)s - %(message)s"
log_format = CustomFormatter(log_format_str, datefmt="%H:%M:%S")

# 3. Setup the File Handler. Its level is controlled by debug_logging_to_file()
#    below -- it starts at INFO and can be raised to DEBUG at runtime.
file_handler = logging.FileHandler(_log_file, encoding="utf-8")
file_handler.setFormatter(log_format)
file_handler.setLevel(logging.INFO)

# 4. Get the root logger and add the file handler.
#    Root stays at DEBUG so that it never filters anything out before it
#    reaches the handlers -- each handler's own level does the filtering.
logger = logging.getLogger()
logger.setLevel(logging.DEBUG)
logger.addHandler(file_handler)

# 5. Console handler: always logs DEBUG and above whenever there IS a
#    console. Unlike the file handler, this has no separate level toggle --
#    see attach_console_handler() below and set_console_visible() for how
#    the console itself gets shown/hidden on Windows.
_console_handler = None


def attach_console_handler():
    """
    (Re)build the console log handler from whatever sys.stdout currently is.

    Called once below at import time for the normal case (running un-frozen,
    or a --console PyInstaller build already attached to a terminal). Call it
    AGAIN after set_console_visible(True) allocates a fresh console and
    repoints sys.stdout, since a handler built at import time is bound to the
    old sys.stdout (often None) and won't pick up the new one on its own.

    Console output always logs DEBUG and above; there's no level toggle for
    it the way there is for the file (see debug_logging_to_file).
    """
    global _console_handler

    if _console_handler is not None:
        logger.removeHandler(_console_handler)
        _console_handler = None

    if sys.stdout is None or not hasattr(sys.stdout, "buffer"):
        return

    _utf8_stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")
    _console_handler = logging.StreamHandler(_utf8_stdout)
    _console_handler.setFormatter(log_format)
    _console_handler.setLevel(logging.DEBUG)
    logger.addHandler(_console_handler)


attach_console_handler()


def set_console_visible(show: bool):
    kernel32 = ctypes.windll.kernel32

    if show:
        if kernel32.GetConsoleWindow():
            return

        if not kernel32.AllocConsole():
            return

        sys.stdout = io.TextIOWrapper(
            open("CONOUT$", "wb", buffering=0), encoding="utf-8", errors="replace", write_through=True
        )
        sys.stderr = io.TextIOWrapper(
            open("CONOUT$", "wb", buffering=0), encoding="utf-8", errors="replace", write_through=True
        )
        sys.stdin = io.TextIOWrapper(open("CONIN$", "rb", buffering=0), encoding="utf-8", errors="replace")

        attach_console_handler()
    else:
        if kernel32.GetConsoleWindow():
            kernel32.FreeConsole()
        attach_console_handler()


def debug_logging_to_file(enabled: bool = False):
    file_handler.setLevel(logging.DEBUG if enabled else logging.INFO)
debug_logging_to_file(False)


def _build_exception_report_path() -> str:
    timestamp = datetime.datetime.now().strftime("%Y-%m-%d_%H-%M-%S_%f")
    return str((LOG_DIR / f"traceback_{timestamp}.log").resolve())


def _extract_traceback_origin(exc_traceback):
    if exc_traceback is None:
        return None, None

    frames = traceback.extract_tb(exc_traceback)
    if not frames:
        return None, None

    last_frame = frames[-1]
    return os.path.abspath(last_frame.filename), last_frame.name


def _write_exception_report(exc_type, exc_value, exc_traceback, context=None):
    report_path = _build_exception_report_path()
    source_path, source_func = _extract_traceback_origin(exc_traceback)

    try:
        with open(report_path, "w", encoding="utf-8") as report_file:
            report_file.write(f"Timestamp: {datetime.datetime.now().isoformat()}\n")
            if context:
                report_file.write(f"Context: {context}\n")
            report_file.write(f"Exception type: {getattr(exc_type, '__name__', type(exc_value).__name__)}\n")
            report_file.write(f"Exception: {exc_value!r}\n")
            if source_path:
                report_file.write(f"Source file: {source_path}\n")
            if source_func:
                report_file.write(f"Source function: {source_func}\n")
            report_file.write("\nTraceback:\n")
            traceback.print_exception(exc_type, exc_value, exc_traceback, file=report_file)
    except OSError as e:
        print(f"[CRITICAL] Failed to write exception report to {report_path}: {e}", file=sys.stderr)
        try:
            # Fallback: write to stderr
            traceback.print_exception(exc_type, exc_value, exc_traceback, file=sys.stderr)
        except Exception:
            pass  # Last resort, at least we tried

    return report_path, source_path, source_func


def log_unhandled_exception(exc_type, exc_value, exc_traceback, context=None):
    report_path, source_path, source_func = _write_exception_report(
        exc_type,
        exc_value,
        exc_traceback,
        context=context,
    )

    extra = None
    if source_path:
        extra = {
            'source_override': source_path,
        }

    logger.error(
        "Unhandled exception: %r | traceback_file=%s",
        exc_value,
        report_path,
        extra=extra,
    )

    return report_path


def _handle_sys_exception(exc_type, exc_value, exc_traceback):
    if issubclass(exc_type, KeyboardInterrupt):
        sys.__excepthook__(exc_type, exc_value, exc_traceback)
        return

    log_unhandled_exception(
        exc_type,
        exc_value,
        exc_traceback,
        context="sys.excepthook",
    )


def _handle_thread_exception(args):
    if issubclass(args.exc_type, KeyboardInterrupt):
        if hasattr(threading, "__excepthook__"):
            threading.__excepthook__(args)
        return

    thread_name = args.thread.name if args.thread else "<unknown>"
    log_unhandled_exception(
        args.exc_type,
        args.exc_value,
        args.exc_traceback,
        context=f"threading.excepthook thread={thread_name}",
    )


sys.excepthook = _handle_sys_exception

if hasattr(threading, "excepthook"):
    threading.excepthook = _handle_thread_exception

# 6. The exported log function
def log(level: str, message: str):
    frame = inspect.currentframe().f_back
    filename = os.path.basename(frame.f_code.co_filename)
    funcname = frame.f_code.co_name

    extra = {'caller_filename': filename, 'caller_funcName': funcname}

    level_upper = level.upper()
    log_func = getattr(logger, level_upper.lower(), logger.info)
    log_func(message, extra=extra)