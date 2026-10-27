#!/usr/bin/env python3
"""Check all CLI installations and reported login without generating AI responses."""
import json
import shutil
import subprocess
import tempfile
from probe_clis import PROVIDERS, run


def authentication(provider, success, stdout):
    if provider == "opencode":
        return "unknown"
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


def parse_opencode_models(text):
    import re
    return list(dict.fromkeys(line.strip() for line in text.splitlines()
                             if re.fullmatch(r"[^\s/]+/[^\s]+", line.strip()) and len(line.strip()) <= 400))


def check(provider, aliases, cwd):
    executable = next((shutil.which(alias) for alias in aliases if shutil.which(alias)), None)
    result = dict(provider=provider, executable=executable, installed=bool(executable),
                  runnable=False, version=None, authentication="not_checked", generation="not_tested", credential_listing="not_checked")
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
        if provider == "opencode":
            # Only report command availability. Never persist account-bearing output.
            try:
                auth = run([executable, "auth", "list"], cwd, timeout=20)
                result["credential_listing"] = "available" if auth["exit_code"] == 0 and not auth["termination"] else "failed"
            except (OSError, TimeoutError, subprocess.SubprocessError):
                result["credential_listing"] = "failed"
            catalog = run([executable, "models"], cwd, timeout=20)
            result["models"] = parse_opencode_models(catalog["stdout"]) if catalog["exit_code"] == 0 and not catalog["termination"] else []
            result["authentication"] = "unknown"
            if catalog["exit_code"] != 0 or catalog["termination"]:
                result["message"] = "OpenCode can start, but model discovery failed"
            elif not result["models"]:
                result["message"] = "No model IDs were recognized; check the CLI version/configuration"
            else:
                result["message"] = "Select a provider/model and test its connection; discovery does not prove access"
            return result
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
