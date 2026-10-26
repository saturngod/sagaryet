# CLI feasibility — October 25, 2026

Status: **in progress; no real provider is approved for LAN use**.

## Current implementation evidence

The Rust adapters now discover all four installed catalogs (Codex 7, Claude 5,
AGY 6, Qoder 15). Codex live generation and HTTP multi-turn history passed.
Qoder live generation passed with a temporary config directory containing only
auth files. A follow-up on 2026-10-26 enabled the hidden
`--include-partial-messages` flag and verified 276 text events: first text at
7.599 seconds, completion at 10.257 seconds (Qoder CLI 1.1.78).
Claude reports configured login, but its live adapter returned
`authentication_required`. AGY generation remains disabled because complete
tool/extension isolation is not established for the installed CLI. Qoder 1.1.78
also supports `status --output json`; its `logged_in` boolean is now used rather
than assuming an undocumented auth subcommand.

The earlier spike table below is retained as historical evidence. Current
commands and remaining release gates are in README and plan.md. See
`implementation-verification.json` for sanitized outcomes.

## Earlier spike

The machine-readable evidence is `cli-probe.json`. It contains version strings,
flag availability, event counts, and outcome categories. It deliberately omits
account identifiers, access tokens, model response bodies, and raw stderr.

| Provider | Installed version | Evidence | Remaining gate |
|---|---|---|---|
| Codex | 0.163.0 | App Server initialized successfully; `model/list` returned 7 entries; clean process exit | Verify a configuration that disables built-in tools, hooks, skills, MCP and inherited instructions before live generation; then streaming, history and turn interruption |
| Claude Code | 2.1.296 | Advertises text deltas, tools restriction, safe mode, MCP isolation and ephemeral sessions. An isolated synthetic request returned an authentication failure, no text deltas, unsuccessful terminal result and exit 1 | Restore CLI authentication, rerun synthetic request, verify inherited extensions are disabled, test history and cancellation |
| AGY | 1.4.7 | Advertises `stream-json` input/output, model, effort and sandbox flags | No complete tool/extension disabling configuration established; generation intentionally not attempted |
| Qoder CLI | 1.1.78 | Help advertises tool restrictions, strict MCP configuration, settings selection and nonpersistent sessions | Verify hooks/plugins/skills isolation; successful generation, partial events, terminal events, quota errors, history and cancellation remain untested |

A model catalog does not establish that inference is authorized. Advertised
stream-json output does not establish incremental assistant-text streaming.
A read-only filesystem sandbox does not prevent all tool execution.

## Reproduce

```sh
bun run probe:clis
# Explicitly enables a small Claude request and may consume quota:
python3 scripts/probe_clis.py --live
python3 -m unittest discover -s tests -v
```

The probe uses a fresh temporary working directory, fixed executable names and
argument arrays, concurrent stdout/stderr reads, a 1 MiB combined output cap,
15-second metadata deadlines and a 60-second live deadline. POSIX processes run
in a new process group, killed at cleanup. Windows uses taskkill `/T /F`; Windows
behavior has not been verified on a Windows host. Detached descendants escaping
the process group are outside this spike's guarantee.

`--live` currently tests Claude only, with safe mode, an empty built-in tool set,
strict empty MCP configuration, slash commands disabled, no settings sources,
hooks disabled, `dontAsk` permissions and no session persistence. Policy-managed
settings can still apply. This is a feasibility probe, not a sandbox guarantee.
The production adapter must pass adversarial tool/extension tests before LAN use.

Tests cover terminal failure recognition, exclusion of reasoning from delta
counts, malformed JSON, split Myanmar UTF-8, concurrent stderr, timeout cleanup,
and output limits. The spike buffers bounded output before analysis; it does not
measure time to first token or prove production backpressure behavior.

## Official references reviewed

- [Codex App Server](https://learn.chatgpt.com/docs/app-server): initialization, model discovery and lifecycle protocol. Also generated the installed binary's JSON schemas into a temporary directory; they are not shipped as a source dependency.
- [Claude programmatic usage](https://code.claude.com/docs/en/headless): print mode and structured results. Local `--help` supplies the installed-version isolation flags.
- [AGY headless mode](https://www.antigravity.google/docs/cli/headless/): structured headless output; no complete isolation recipe established.
- [Qoder scripting](https://docs.qoder.com/cli/run-in-scripts): structured input/output. [Hook configuration](https://docs.qoder.com/cli/hooks) and [settings precedence](https://docs.qoder.com/cli/settings) require explicit isolation review.
