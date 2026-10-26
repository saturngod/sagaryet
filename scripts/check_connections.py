#!/usr/bin/env python3
"""Check all CLI installations and reported login without generating AI responses."""
import json
import shutil
import subprocess
import tempfile
from probe_clis import PROVIDERS, run


def authentication(provider, success, stdout):
    if provider == "codex":
        return "reported_authenticated" if success else "unknown"
    try:
        value = json.loads(stdout)
    except (ValueError, TypeError):
        return "unknown"
    if not isinstance(value, dict):
        return "unknown"
    logged_in = value.get("loggedIn" if provider == "claude" else "logged_in")
    if success and logged_in is True:
        return "reported_authenticated"
    if logged_in is False and (success if provider == "qoder" else not success):
        return "reported_signed_out"
    return "unknown"


def check(provider, aliases, cwd):
    executable = next((shutil.which(alias) for alias in aliases if shutil.which(alias)), None)
    result = dict(provider=provider, executable=executable, installed=bool(executable),
                  runnable=False, version=None, authentication="not_checked", generation="not_tested")
    if not executable:
        return result
    try:
        version = run([executable, "--version"], cwd, timeout=15)
        result["runnable"] = version["exit_code"] == 0 and not version["termination"]
        if not result["runnable"]:
            result["message"] = "Version check failed"
            return result
        lines = (version["stdout"] or version["stderr"]).splitlines()
        result["version"] = next((line.strip()[:160] for line in lines if line.strip()), None)
        args = {"codex": ["login", "status"], "claude": ["auth", "status"],
                "qoder": ["status", "--output", "json"]}.get(provider)
        result["authentication"] = "unknown"
        if args:
            auth = run([executable, *args], cwd, timeout=15)
            if not auth["termination"]:
                result["authentication"] = authentication(provider, auth["exit_code"] == 0, auth["stdout"])
        result["message"] = "No AI prompt sent; live generation is not tested"
    except (OSError, TimeoutError, subprocess.SubprocessError):
        result["message"] = "CLI check could not complete"
    return result


def main():
    with tempfile.TemporaryDirectory(prefix="sagaryet-check-") as cwd:
        results = [check(provider, aliases, cwd) for provider, aliases in PROVIDERS.items()]
    # Never print raw auth responses: they can contain account identifiers.
    print(json.dumps({"providers": results}, indent=2))


if __name__ == "__main__":
    main()
