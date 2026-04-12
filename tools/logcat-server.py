#!/usr/bin/env python3
"""
Logcat HTTP server — run on your laptop, read from anywhere.

Usage:
    python3 logcat-server.py [--port 9999] [--lines 5000]

Endpoints:
    GET /           — last 200 lines (default)
    GET /all        — all captured lines
    GET /tail?n=500 — last N lines
    GET /crash      — only lines with crash/error keywords
    GET /clear      — clear buffer
    GET /status     — buffer stats

Requires: adb in PATH, device connected via USB.
"""

import subprocess
import threading
import sys
import argparse
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs
from collections import deque

# Global log buffer
log_buffer = deque(maxlen=10000)
lock = threading.Lock()


def run_logcat():
    """Run adb logcat and capture output into the ring buffer."""
    # Clear existing logcat first
    subprocess.run(["adb", "logcat", "-c"], capture_output=True)

    proc = subprocess.Popen(
        ["adb", "logcat", "-v", "threadtime",
         "--pid", get_app_pid(),
        ] if get_app_pid() else [
            "adb", "logcat", "-v", "threadtime",
            "AndroidRuntime:E", "System.err:W", "wzp:V",
            "WzpEngine:V", "CallActivity:V", "DEBUG:V",
            "linker:E", "art:E", "*:S",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )

    for line in iter(proc.stdout.readline, ""):
        line = line.rstrip("\n")
        with lock:
            log_buffer.append(line)

    proc.wait()


def get_app_pid():
    """Try to get PID of com.wzp.phone."""
    try:
        result = subprocess.run(
            ["adb", "shell", "pidof", "com.wzp.phone"],
            capture_output=True, text=True, timeout=3,
        )
        pid = result.stdout.strip()
        if pid and pid.isdigit():
            return pid
    except Exception:
        pass
    return None


def run_logcat_unfiltered():
    """Fallback: capture everything, filter in Python."""
    subprocess.run(["adb", "logcat", "-c"], capture_output=True)

    proc = subprocess.Popen(
        ["adb", "logcat", "-v", "threadtime"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )

    keywords = [
        "wzp", "WzpEngine", "CallActivity", "CallViewModel",
        "AndroidRuntime", "FATAL", "dlopen", "UnsatisfiedLink",
        "Signal", "DEBUG", "linker", "libc++", "libwzp",
        "com.wzp", "crash", "SIGSEGV", "SIGABRT", "backtrace",
        "native", "art", "JNI",
    ]

    for line in iter(proc.stdout.readline, ""):
        line = line.rstrip("\n")
        lower = line.lower()
        if any(k.lower() in lower for k in keywords):
            with lock:
                log_buffer.append(line)

    proc.wait()


class LogHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        params = parse_qs(parsed.query)

        if path == "/clear":
            with lock:
                log_buffer.clear()
            self.respond(200, "Buffer cleared\n")

        elif path == "/status":
            with lock:
                count = len(log_buffer)
            self.respond(200, f"Lines buffered: {count}\nMax: {log_buffer.maxlen}\n")

        elif path == "/crash":
            crash_keywords = [
                "fatal", "crash", "exception", "sigsegv", "sigabrt",
                "unsatisfiedlink", "dlopen", "backtrace", "signal",
                "androidruntime", "error", "panic",
            ]
            with lock:
                lines = [
                    l for l in log_buffer
                    if any(k in l.lower() for k in crash_keywords)
                ]
            self.respond(200, "\n".join(lines) + "\n")

        elif path == "/all":
            with lock:
                lines = list(log_buffer)
            self.respond(200, "\n".join(lines) + "\n")

        else:
            # Default: /tail?n=200 or just /
            n = int(params.get("n", [200])[0])
            with lock:
                lines = list(log_buffer)[-n:]
            self.respond(200, "\n".join(lines) + "\n")

    def respond(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def log_message(self, format, *args):
        pass  # suppress request logging


def main():
    parser = argparse.ArgumentParser(description="Logcat HTTP server")
    parser.add_argument("--port", type=int, default=9999)
    parser.add_argument("--lines", type=int, default=10000, help="Max buffer size")
    parser.add_argument("--unfiltered", action="store_true", help="Capture all logcat, filter in Python")
    args = parser.parse_args()

    global log_buffer
    log_buffer = deque(maxlen=args.lines)

    # Start logcat capture thread
    target = run_logcat_unfiltered if args.unfiltered else run_logcat
    t = threading.Thread(target=run_logcat_unfiltered, daemon=True)
    t.start()

    server = HTTPServer(("0.0.0.0", args.port), LogHandler)
    print(f"Logcat server on http://0.0.0.0:{args.port}")
    print(f"  GET /           — last 200 lines")
    print(f"  GET /tail?n=500 — last N lines")
    print(f"  GET /crash      — crash/error lines only")
    print(f"  GET /all        — full buffer")
    print(f"  GET /clear      — clear buffer")
    print(f"")
    print(f"Now open the WZP app on your phone and reproduce the crash.")
    print(f"Then share: http://<your-laptop-ip>:{args.port}/crash")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nStopped.")


if __name__ == "__main__":
    main()
