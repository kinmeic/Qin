#!/usr/bin/env python3
"""Exercise TUI terminal ownership and approval cancellation with a local model.

Usage: python3 scripts/tui-smoke.py [path/to/qin]
Uses only the Python standard library; no credentials or remote services.
"""
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import threading
import time


def call(identifier, command, **kwargs):
    return {"id": identifier, "type": "function", "function": {
        "name": "shell", "arguments": json.dumps({"command": command, **kwargs})}}


class Model(http.server.BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(request)
        turn = sum(message["role"] == "user" for message in request["messages"])
        if turn == 1 and not any(message["role"] == "tool" for message in request["messages"]):
            message = {"role": "assistant", "content": None, "tool_calls": [
                call("captured", "test ! -t 0 && printf 'QIN_CAPTURED\\n'"),
                call("terminal", "printf 'QIN_PROMPT\\n'; read answer; test \"$answer\" = terminal-answer && test -t 0 && test -t 1 && test -t 2 && printf '\\033[32mQIN_DIRECT\\033[0m\\n'", interactive=True),
            ]}
        elif turn == 1:
            message = {"role": "assistant", "content": "QIN_FINISHED"}
        else:
            message = {"role": "assistant", "content": None, "tool_calls": [
                call(f"approval-{turn}", "rm protected-file"),
                call(f"later-{turn}", "printf unexpected > must-not-exist"),
            ]}
        body = json.dumps({"choices": [{"message": message, "finish_reason":
                                      "tool_calls" if message.get("tool_calls") else "stop"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/qin").resolve()
    if not binary.is_file():
        raise SystemExit(f"Build qin first: binary missing at {binary}")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    pid = None
    descriptor = None
    try:
        with tempfile.TemporaryDirectory(prefix="qin-tui-smoke-") as temporary:
            directory = Path(temporary)
            config = directory / "config.toml"
            config.write_text(f'''version = 1
default_model = "primary"
[models.primary]
base_url = "http://127.0.0.1:{server.server_port}/v1"
model = "smoke-model"
api_key = "local-test-only"
stream = false
[storage]
enabled = false
data_dir = "{directory}"
[agent]
wall_time_seconds = 5
''')
            config.chmod(0o600)
            (directory / "protected-file").write_text("keep")
            pid, descriptor = pty.fork()
            if pid == 0:
                os.chdir(directory)
                os.environ["TERM"] = "xterm-256color"
                os.execv(str(binary), [str(binary), "--config", str(config), "--yes", "--quiet", "tui"])
            thread.start()
            fcntl.ioctl(descriptor, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 120, 0, 0))
            observed = bytearray()
            answered_queries = 0

            def until(marker, timeout=10):
                nonlocal answered_queries
                deadline = time.monotonic() + timeout
                start = len(observed)
                while time.monotonic() < deadline:
                    plain = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", observed[start:])
                    if marker.replace(b" ", b"") in plain.replace(b" ", b""):
                        return
                    ready, _, _ = select.select([descriptor], [], [], 0.1)
                    if ready:
                        try:
                            chunk = os.read(descriptor, 65536)
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                            chunk = b""
                        if not chunk:
                            raise AssertionError(f"TUI exited before {marker!r}; tail={observed[-4000:]!r}")
                        observed.extend(chunk)
                        # A real terminal answers DSR cursor-position requests.
                        queries = observed.count(b"\x1b[6n")
                        for _ in range(queries - answered_queries):
                            os.write(descriptor, b"\x1b[1;1R")
                        answered_queries = queries
                raise AssertionError(f"Timed out waiting for {marker!r}; tail={observed[-1000:]!r}")

            until(b"Interactive agent")
            os.write(descriptor, b"terminal smoke\r")
            until(b"QIN_PROMPT")
            os.write(descriptor, b"terminal-answer\n")
            until(b"QIN_FINISHED")
            assert b"\x1b[32mQIN_DIRECT\x1b[0m" in observed, "Interactive stdout lost TTY/control sequences"
            assert b"QIN_CAPTURED" in observed, "Ordinary output missing from TUI"
            messages = Model.requests[-1]["messages"]
            results = {message["tool_call_id"]: message["content"] for message in messages if message["role"] == "tool"}
            assert results["captured"].startswith("exit_code=0\nstdout: QIN_CAPTURED")
            assert results["terminal"].startswith("exit_code=0\n[Interactive command output")
            os.write(descriptor, b"cancel approval\r")
            until(b"Allow once")
            os.write(descriptor, b"\x03")
            until(b"Turn cancelled by the user")
            os.write(descriptor, b"expire approval\r")
            until(b"Allow once")
            until(b"Turn ended with an error")
            os.write(descriptor, b"/exit\r")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                ended, status = os.waitpid(pid, os.WNOHANG)
                if ended:
                    pid = None
                    assert os.waitstatus_to_exitcode(status) == 0, status
                    break
                ready, _, _ = select.select([descriptor], [], [], 0.05)
                if ready:
                    try:
                        os.read(descriptor, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
            assert pid is None, "TUI did not shut down"
            assert (directory / "protected-file").read_text() == "keep"
            assert not (directory / "must-not-exist").exists()
            state = json.loads((directory / "qin-session.json").read_text())
            messages = [entry["message"] for entry in state["session"]["messages"]]
            tool_results = {message["tool_call_id"]: message for message in messages if message["role"] == "tool"}
            assert set(tool_results) == {"captured", "terminal", "approval-2", "later-2", "approval-3", "later-3"}
            print("TUI smoke passed: captured output, terminal stdio/ANSI, cancel, approval deadline, paired results, shutdown")
    finally:
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
        if descriptor is not None:
            os.close(descriptor)
        if pid is not None:
            os.waitpid(pid, 0)
        if thread.is_alive():
            server.shutdown()
            thread.join()
        server.server_close()


if __name__ == "__main__":
    main()
