# Sagaryet

A desktop bridge for OpenAI-compatible text chat through installed AI CLIs.
**Localhost and local-network development build; the full release plan is still in progress.**

## Run

Requires Bun, Rust, and the platform prerequisites for Tauri 2.

```sh
bun install
bun run tauri dev
```

1. Select your installed CLI.
2. Leave the API key empty, or enter your own key.
3. Click **Start server**. The server starts at port **9693**, trying 9694, 9695, and so on if a port is already in use.
4. Copy the API URL, key (if set), and model name into your AI app.

The server exposes all available models for the chosen CLI. Choose the model in your AI client for each request. Stop the server to change the CLI or key.
The Connection section shows one URL for this computer and one per detected
local IPv4 address, so a phone on the same network can use the local-network
URL. Traffic is unencrypted HTTP. An empty key requires no authentication, which
means anyone on that network can use the server; set a key before sharing the URL.
**API keys & fields** contains a compact reference for client settings and request
fields. Omit `reasoning_effort` to use the provider default, or supply an effort supported by the requested model.
Keys, prompts, and responses are not persisted by Sagaryet. Provider software may
have its own diagnostics and retention behavior.

For terminal installation/login checks (no AI prompts):

```sh
bun run check:clis
```

This command uses your terminal PATH and prints sanitized JSON. The desktop
checker also searches common per-user installation directories. Neither checker
confuses a configured login with a successful AI response.

## Provider support verified on this Mac

| Provider | Discovery | Login check | Generation |
|---|---|---|---|
| Codex | 7 models; native effort metadata | `login status` | Live adapter and HTTP multi-turn check passed |
| Claude Code | 5 models; native effort metadata | `auth status` JSON | Adapter implemented; live check returned authentication failure |
| AGY | 6 models from `agy models` | Unknown | Disabled: complete tool/extension isolation not established |
| Qoder | 15 models; native effort metadata | `status --output json` | Live check passed; incremental text streaming |

Counts and account availability can change. A successful connection test applies
to the tested model at that time. It does not guarantee future access.

Codex uses an ephemeral App Server thread in a temporary config root with only
CLI auth copied in and tool-related features disabled. Claude uses safe mode,
empty tools and MCP, disabled hooks/skills, and no session persistence. Qoder uses
an isolated config root with only auth files copied, empty tools/MCP and no
session persistence. Temporary auth copies are removed when execution ends.
All adapters remain subject to the isolation checks described here; LAN access is
enabled and unencrypted, so set an access key before exposing the server to a network.
AGY discovery does not run generation or load a chat agent.

## API compatibility

| Feature | Behavior |
|---|---|
| `GET /healthz` | Minimal liveness; no key required |
| `GET /v1/models` | All available models for the chosen CLI; bearer key required only when configured |
| `POST /v1/chat/completions` | Buffered JSON or SSE; any available model for the chosen CLI |
| Messages | Optional initial system message; alternating user/assistant; final user; 1–128 messages |
| Content | Nonempty text strings only; 64 KiB request limit |
| `n` | Omitted or `1` |
| `reasoning_effort` | Per-request supported effort; omit to use the provider default |
| Other fields | Rejected, including tools, temperature, token limits and structured output |
| Streaming | Incremental Codex/Claude/Qoder text deltas; `stream: true` returns OpenAI-compatible SSE |
| SSE failure | Error payload, then close without successful finish or `[DONE]` |
| Usage | Not exposed yet |
| History | Fresh CLI session; conversation serialized as role/content JSON; hierarchy may differ from native APIs |
| Concurrency | One active request; four waiting; further requests receive 429 |
| Deadlines | 30-second queue wait; 120-second generation limit |
| Output | 4 MiB CLI stdout cap; 1 MiB answer cap; bounded channel of 16 events |
| Rate limit | 60 authenticated requests/minute across clients |
| Browser access | Origin-bearing requests rejected; no CORS allowance |
| Network | `0.0.0.0` (this computer and local IPv4); Host must match localhost, 127.0.0.1, or a current local IPv4 address plus the server port |
| Cancellation | Disconnect, desktop Cancel, Stop, timeout and app teardown clean up execution |

```sh
curl http://127.0.0.1:PORT/v1/chat/completions \
  -H 'Authorization: Bearer YOUR_ACCESS_KEY' \
  -H 'Content-Type: application/json' \
  -d '{"model":"codex/MODEL_ID","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Settings are snapshotted at server startup; stop before changing them. Stopping
the server cancels active and waiting requests. There are no HTTP
administration endpoints. Raw provider diagnostics and private reasoning are
not returned to clients.

## Verify

```sh
bun run build
bun run test
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
bun run tauri build --no-bundle
# Installed discovery, no inference:
cargo test --manifest-path src-tauri/Cargo.toml installed_all_catalogs -- --ignored --nocapture
# Local network address reachability:
cargo test --manifest-path src-tauri/Cargo.toml lan_address_serves_models -- --ignored --nocapture
# Explicit live tests, consume quota:
SAGARYET_TEST_MODEL=codex/gpt-5.6-sol cargo test --manifest-path src-tauri/Cargo.toml installed_live_connection -- --ignored --nocapture
SAGARYET_TEST_MODEL=qoder/auto cargo test --manifest-path src-tauri/Cargo.toml installed_live_connection -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml installed_codex_http_history -- --ignored --nocapture
```

See [plan](plan.md), [feasibility](docs/feasibility.md), and
[verification evidence](docs/implementation-verification.json). GitHub Actions
runs the same verification workflow for branch pushes, pull requests, manual runs,
and release tags. It tests real HTTP sockets (health, authentication, JSON/SSE,
queueing, cancellation, and stop/restart) without installed provider CLIs, then
builds and uploads native installers:

| Platform | Architecture | Installers |
|---|---|---|
| macOS | Apple Silicon and Intel | DMG |
| Windows | x64 | NSIS EXE and MSI |
| Linux (Ubuntu 22.04) | x64 | DEB and AppImage |

Tagged releases stay in draft until verification and every release build succeed.
Bun 1.3.0 is pinned in CI; dependency installs use the committed lockfiles.
The HTTP server is embedded in the desktop app and needs no separate Node,
Bun, or Python runtime after installation. Keep the app running while using the API.
Allow the app through the OS firewall for LAN clients; localhost needs no LAN rule.
Remote Windows/Linux results still need to be confirmed by running GitHub Actions.
Windows batch-only CLI shims are currently rejected; a native executable is required.

Remaining release gates include AGY generation isolation, restoring Claude live
authentication, adversarial tool/hook/extension checks, LAN isolation and phone
streaming verification, OS credential-store integration, packaged
signing/notarization and verified Windows/Linux/mobile-client combinations.
This build must not be advertised as release-complete.
