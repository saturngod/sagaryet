#!/usr/bin/env python3
"""Bounded local feasibility probes. Raw output and credentials are never persisted."""
import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import subprocess
import tempfile
import threading
import time

LIMIT = 1024 * 1024
PROVIDERS = {"codex": ["codex"], "claude": ["claude"],
             "qoder": ["qodercli", "qoder"]}


def terminate(process):
    if os.name == "posix":
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    else:
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                       capture_output=True, timeout=5, check=False)
    process.wait(timeout=5)


def run(command, cwd, timeout=15, stdin=None, protocol=None):
    process = subprocess.Popen(command, cwd=cwd, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=os.name == "posix")
    events = queue.Queue(maxsize=256)
    stop = threading.Event()
    def read(pipe, channel):
        try:
            while not stop.is_set():
                chunk = os.read(pipe.fileno(), 4096)
                if not chunk:
                    break
                while not stop.is_set():
                    try:
                        events.put((channel, chunk), timeout=.1)
                        break
                    except queue.Full:
                        pass
        finally:
            pipe.close()
    readers = [threading.Thread(target=read, args=(pipe, name), daemon=True)
               for pipe, name in [(process.stdout, "stdout"), (process.stderr, "stderr")]]
    for reader in readers:
        reader.start()
    output = {"stdout": bytearray(), "stderr": bytearray()}
    started = time.monotonic()
    reason = None
    try:
        if stdin is not None:
            process.stdin.write(stdin.encode())
        if protocol is None:
            process.stdin.close()
        else:
            process.stdin.flush()
        while any(t.is_alive() for t in readers) or not events.empty() or process.poll() is None:
            if time.monotonic() - started > timeout:
                reason = "timeout"
                break
            try:
                channel, chunk = events.get(timeout=.05)
            except queue.Empty:
                continue
            if sum(map(len, output.values())) + len(chunk) > LIMIT:
                reason = "output_limit"
                break
            output[channel].extend(chunk)
            if channel == "stdout" and protocol is not None:
                protocol(chunk, process.stdin)
    except BrokenPipeError:
        reason = "stdin_closed"
    finally:
        stop.set()
        try:
            process.stdin.close()
        except BrokenPipeError:
            pass
        terminate(process)  # Also remove descendants after the direct process exits.
        for reader in readers:
            reader.join(timeout=1)
    return {"exit_code": process.returncode, "termination": reason,
            **{key: bytes(value).decode("utf-8", errors="replace") for key, value in output.items()}}


def summarize_stream(text):
    """Record event types/counts, never model text or account-bearing envelopes."""
    kinds = {}
    deltas = 0
    terminal = False
    success = False
    invalid = 0
    failure_category = None
    for line in text.splitlines():
        try:
            event = json.loads(line)
            if not isinstance(event, dict):
                raise ValueError("not an object")
        except (ValueError, TypeError):
            invalid += 1
            continue
        kind = event.get("type", "unknown")
        kinds[kind] = kinds.get(kind, 0) + 1
        inner = event.get("event", {})
        if kind == "stream_event" and inner.get("type") == "content_block_delta":
            delta = inner.get("delta", {})
            if delta.get("type") == "text_delta" and delta.get("text"):
                deltas += 1
        if kind == "result":
            terminal = True
            success = event.get("subtype") == "success" and not event.get("is_error", False)
            if not success:
                # Classify internally, without saving provider text or identifiers.
                error = json.dumps(event).lower()
                failure_category = next((category for category, needles in {
                    "authentication": ["login", "log in", "authentication", "unauthorized", "oauth", "api key"],
                    "quota": ["quota", "credit", "rate limit", "usage limit"],
                    "network": ["connection", "network", "fetch failed"],
                }.items() if any(needle in error for needle in needles)), "provider_error")
    return {"event_types": kinds, "text_delta_count": deltas,
            "successful_terminal": terminal and success, "invalid_records": invalid,
            "failure_category": failure_category}


def probe_codex(executable, cwd):
    pending = bytearray()
    summary = {"initialized": False, "model_count": None, "protocol_errors": 0}
    def protocol(chunk, pipe):
        pending.extend(chunk)
        while b"\n" in pending:
            line, _, rest = pending.partition(b"\n")
            pending[:] = rest
            try:
                event = json.loads(line)
            except ValueError:
                summary["protocol_errors"] += 1
                continue
            if event.get("id") == 0:
                if "error" in event:
                    summary["protocol_errors"] += 1
                    pipe.close()
                    return
                summary["initialized"] = True
                for request in [{"method": "initialized"},
                                {"id": 1, "method": "model/list", "params": {}}]:
                    pipe.write((json.dumps(request) + "\n").encode())
                pipe.flush()
            elif event.get("id") == 1:
                if "error" in event:
                    summary["protocol_errors"] += 1
                else:
                    summary["model_count"] = len(event.get("result", {}).get("data", []))
                pipe.close()
    initial = {"id": 0, "method": "initialize", "params": {
        "clientInfo": {"name": "sagaryet-feasibility", "version": "0.1.0"}}}
    result = run([executable, "app-server"], cwd, stdin=json.dumps(initial) + "\n", protocol=protocol)
    return {**summary, "exit_code": result["exit_code"], "termination": result["termination"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="Run a small Claude request; consumes quota")
    parser.add_argument("--output", type=Path, default=Path("docs/cli-probe.json"))
    args = parser.parse_args()
    report = {"schema_version": 1, "platform": os.sys.platform, "providers": {}}
    with tempfile.TemporaryDirectory(prefix="sagaryet-probe-") as cwd:
        for provider, aliases in PROVIDERS.items():
            executable = next((shutil.which(alias) for alias in aliases if shutil.which(alias)), None)
            entry = {"installed": bool(executable), "lan_ready": False}
            report["providers"][provider] = entry
            if not executable:
                continue
            version = run([executable, "--version"], cwd)
            help_result = run([executable, "--help"], cwd)
            help_text = help_result["stdout"] + help_result["stderr"]
            entry.update(version=version["stdout"].strip().splitlines()[:1],
                         version_exit_code=version["exit_code"],
                         help_exit_code=help_result["exit_code"],
                         advertised_flags=[flag for flag in ["--tools", "--safe-mode", "--sandbox",
                             "--input-format", "--output-format", "--include-partial-messages",
                             "--strict-mcp-config", "--no-session-persistence", "--model", "--effort",
                             "--reasoning-effort"] if flag in help_text],
                         live_status="not_run")
            if provider == "codex":
                entry["app_server"] = probe_codex(executable, cwd)
            if provider == "claude" and args.live:
                required = ["--safe-mode", "--tools", "--strict-mcp-config", "--disable-slash-commands"]
                if not all(flag in help_text for flag in required):
                    entry["live_status"] = "unsupported_isolation_flags"
                    continue
                result = run([executable, "--safe-mode", "--tools", "", "--strict-mcp-config",
                    "--mcp-config", '{"mcpServers":{}}', "--disable-slash-commands",
                    "--setting-sources", "", "--settings", '{"disableAllHooks":true}',
                    "--permission-mode", "dontAsk", "--no-session-persistence", "--verbose",
                    "--include-partial-messages", "--output-format", "stream-json", "-p"], cwd,
                    timeout=60, stdin="Reply with exactly: Hello မင်္ဂလာပါ. Do not use tools.")
                entry["live"] = {"exit_code": result["exit_code"], "termination": result["termination"],
                                 **summarize_stream(result["stdout"])}
                entry["live_status"] = "passed" if (entry["live"]["successful_terminal"] and
                    result["exit_code"] == 0) else "failed_or_unavailable"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
