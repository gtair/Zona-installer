"""Resolve ModDB mirror links using a real Edge (WebView2) browser instance.
Usage: python webview_resolver.py <moddb_url>
On success, prints "__RESOLVER_RESULT__OK:<json>" where json is
{"url": ..., "referer": ..., "cookie": ...} and exits 0.
On failure, prints "__RESOLVER_RESULT__ERROR:<message>" and exits 1.
"""

import json
import re
import sys
import time

RESULT_MARKER = "__RESOLVER_RESULT__"

MIRROR_PATTERN = re.compile(r"(https://www\.moddb\.com/downloads/mirror/\d+/\d+/[a-f0-9]+)")
START_LINK_PATTERN = re.compile(r'href="[^"]*?/downloads/start/(\d+)"')

POLL_INTERVAL = 0.25        # seconds between checks of the page state
NAV_SETTLE_TIME = 0.6       # url/content must be unchanged this long before we trust it
HARD_TIMEOUT = 40.0         # ceiling inside the worker; the parent enforces its own too
DEFAULT_REFERER = "https://www.moddb.com/"


def _collect_cookie_header(window) -> str:
    """Best-effort extraction of cookies from the resolving session.

    pywebview's get_cookies() isn't available/reliable on every backend
    version, so this must never be allowed to fail the whole resolution -
    a missing cookie header just means we fall back to Referer + UA alone.
    """
    try:
        cookies = window.get_cookies()
    except Exception:
        return ""

    parts = []
    for cookie in cookies or []:
        try:
            for key, morsel in cookie.items():
                parts.append(f"{key}={morsel.value}")
        except AttributeError:
            if isinstance(cookie, dict) and "name" in cookie and "value" in cookie:
                parts.append(f"{cookie['name']}={cookie['value']}")
    return "; ".join(parts)


def _run(start_url: str) -> dict:
    import webview  # deferred: only the subprocess needs this dependency

    state = {"error": None, "result": None, "referer": None, "cookie": ""}
    window = webview.create_window("resolver", start_url, hidden=True)

    def worker():
        try:
            deadline = time.time() + HARD_TIMEOUT
            last_url = None
            last_moddb_url = start_url
            last_change = time.time()

            while time.time() < deadline:
                time.sleep(POLL_INTERVAL)

                try:
                    current_url = window.get_current_url()
                    html = window.evaluate_js("document.documentElement.outerHTML")
                except Exception:
                    # window/DOM not ready yet, or mid-navigation - just retry
                    continue

                if current_url != last_url:
                    last_url = current_url
                    last_change = time.time()
                    if current_url and "moddb.com" in current_url:
                        last_moddb_url = current_url
                    continue  # url just changed - let any further redirect happen

                if time.time() - last_change < NAV_SETTLE_TIME:
                    continue  # not settled long enough to trust the content yet

                if current_url and "/downloads/mirror/" in current_url:
                    state["result"] = current_url
                    state["referer"] = last_moddb_url
                    state["cookie"] = _collect_cookie_header(window)
                    return

                mirror_match = MIRROR_PATTERN.search(html or "")
                if mirror_match:
                    state["result"] = mirror_match.group(1)
                    state["referer"] = current_url or last_moddb_url
                    state["cookie"] = _collect_cookie_header(window)
                    return

                start_match = START_LINK_PATTERN.search(html or "")
                if start_match and "/downloads/start/" not in (current_url or ""):
                    next_url = f"https://www.moddb.com/downloads/start/{start_match.group(1)}"
                    window.load_url(next_url)
                    last_url = None  # force a fresh settle cycle for the new page
                    continue

            state["error"] = f"timed out resolving mirror link (last url: {last_url})"
        except Exception as exc:
            state["error"] = f"{type(exc).__name__}: {exc}"
        finally:
            try:
                window.destroy()
            except Exception:
                pass

    # gui="edgechromium" pins the backend explicitly - on Windows this is the
    # WebView2 runtime (present on Win10 1809+ / all Win11). Without pinning
    # it, pywebview will probe for other backends first, which is slower and
    # is itself a source of inconsistent behaviour across machines.
    # private_mode=True gives each resolution a clean profile, so a corrupted
    # or half-authenticated profile from a previous run can never be the
    # cause of a failure.
    webview.start(worker, gui="edgechromium", private_mode=True)

    if state["error"]:
        raise RuntimeError(state["error"])
    if not state["result"]:
        raise RuntimeError("resolver finished without a result or an error")
    return {
        "url": state["result"],
        "referer": state["referer"] or DEFAULT_REFERER,
        "cookie": state["cookie"],
    }


def main():
    if len(sys.argv) != 2:
        print(f"{RESULT_MARKER}ERROR:usage: webview_resolver.py <url>", flush=True)
        sys.exit(2)

    try:
        resolved = _run(sys.argv[1])
        print(f"{RESULT_MARKER}OK:{json.dumps(resolved)}", flush=True)
        sys.exit(0)
    except Exception as exc:
        print(f"{RESULT_MARKER}ERROR:{exc}", flush=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
