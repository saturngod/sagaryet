//! OpenCode v2 native API. Each operation owns an authenticated loopback child
//! and disposable XDG state. User plugins/configuration and OAuth are not imported.
use crate::{
    adapter::{Event, Message},
    catalog::{Model, ReasoningOption},
    process::{self, Process},
};
use futures_util::StreamExt;
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

const LIMIT: usize = 4 * 1024 * 1024;
const CONFIG: &str = r#"{"share":"disabled","update":"disable","snapshots":false,"plugins":["-*","opencode.models.dev","opencode.provider.*","opencode.agent","opencode.config.agent"],"permissions":[{"action":"*","resource":"*","effect":"deny"}],"policies":[{"action":"tool.use","resource":"*","effect":"deny"}],"agents":{"sagaryet":{"mode":"primary","system":"You are a text chat assistant. Never use tools.","permissions":[{"action":"*","resource":"*","effect":"deny"}]}},"default_agent":"sagaryet"}"#;

fn supported(version: &str) -> bool {
    version
        .split_whitespace()
        .any(|part| part.trim_start_matches('v').starts_with("2."))
}
fn source_data() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| process::home_directory().map(|p| p.join(".local/share")))
        .map(|p| p.join("opencode"))
}
fn managed_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        let mut paths = vec![
            PathBuf::from("/Library/Application Support/opencode"),
            PathBuf::from("/Library/Managed Preferences/ai.opencode.managed.plist"),
        ];
        if let Some(user) = std::env::var_os("USER") {
            paths.push(
                PathBuf::from("/Library/Managed Preferences")
                    .join(user)
                    .join("ai.opencode.managed.plist"),
            );
        }
        paths
    } else if cfg!(windows) {
        vec![
            std::env::var_os("ProgramData")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
                .join("opencode"),
        ]
    } else {
        vec![PathBuf::from("/etc/opencode")]
    }
}
// Import API keys only, never an entire database (which also contains conversations).
fn api_keys(source: &Path) -> Result<Value, String> {
    let mut keys = serde_json::Map::new();
    let legacy = source.join("auth.json");
    if legacy.is_file() {
        let file = std::fs::File::open(legacy).map_err(|_| "Cannot read OpenCode credentials")?;
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read OpenCode credentials")?;
        if bytes.len() > LIMIT {
            return Err("OpenCode credentials exceed limit".into());
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid OpenCode credentials")?;
        if let Some(entries) = value.as_object() {
            for (id, v) in entries {
                if v["type"] == "api"
                    && let Some(key) = v["key"].as_str()
                {
                    keys.insert(id.clone(), json!({"type":"api","key":key}));
                }
            }
        }
    }
    let database = source.join("opencode.db");
    if database.is_file() {
        let db = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "Cannot read OpenCode credential database")?;
        db.busy_timeout(Duration::from_secs(1))
            .map_err(|_| "Cannot read OpenCode credential database")?;
        let mut stmt = db.prepare("SELECT integration_id, value FROM credential WHERE active = 1 OR active IS NULL ORDER BY time_created LIMIT 501")
            .map_err(|_| "Unsupported OpenCode credential database; use provider environment API keys")?;
        let mut rows = stmt
            .query([])
            .map_err(|_| "Cannot read OpenCode credential database")?;
        let mut count = 0;
        let mut total = 0;
        while let Some(row) = rows
            .next()
            .map_err(|_| "Cannot read OpenCode credential database")?
        {
            count += 1;
            let id: String = row
                .get(0)
                .map_err(|_| "Invalid OpenCode credential record")?;
            let raw: String = row
                .get(1)
                .map_err(|_| "Invalid OpenCode credential record")?;
            total += id.len() + raw.len();
            if count > 500 || total > LIMIT {
                return Err("OpenCode credentials exceed limit".into());
            }
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| "Invalid OpenCode credential record")?;
            // A newer OAuth selection must not resurrect an older API key.
            keys.remove(&id);
            if value["type"] == "key"
                && let Some(key) = value["key"].as_str()
            {
                keys.insert(id, json!({"type":"api","key":key}));
            }
        }
    }
    Ok(Value::Object(keys))
}

struct Server {
    // Child drops before the directory, including on task cancellation.
    _process: Process,
    _root: tempfile::TempDir,
    client: Client,
    base: String,
    password: String,
}
impl Server {
    async fn start() -> Result<Self, String> {
        let (ok, version) = process::capture("opencode", &["--version"]).await?;
        if !ok || !supported(&version) {
            return Err("Unsupported OpenCode version: v2 required".into());
        }
        if managed_paths().iter().any(|p| p.exists()) {
            return Err("OpenCode managed configuration prevents isolated execution".into());
        }
        let root = tempfile::tempdir().map_err(|_| "Cannot isolate OpenCode")?;
        let config: Value = serde_json::from_str(CONFIG).unwrap();
        let data = root.path().join("data/opencode");
        std::fs::create_dir_all(&data).map_err(|_| "Cannot isolate OpenCode")?;
        let keys = source_data()
            .map(|p| api_keys(&p))
            .transpose()?
            .unwrap_or_else(|| json!({}));
        std::fs::write(data.join("auth.json"), serde_json::to_vec(&keys).unwrap())
            .map_err(|_| "Cannot load OpenCode credentials")?;
        let password = crate::server::generate_key();
        let mut command = process::command("opencode")?;
        // Only transport settings and provider API credentials survive. In particular,
        // no OPENCODE_* overrides, NODE_OPTIONS, preload hooks, or project PWD.
        command.env_clear();
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TEMP",
            "TMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GEMINI_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENCODE_API_KEY",
            "GROQ_API_KEY",
            "XAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "MISTRAL_API_KEY",
            "CEREBRAS_API_KEY",
            "TOGETHER_API_KEY",
            "OLLAMA_HOST",
            "OLLAMA_BASE_URL",
            "LM_STUDIO_BASE_URL",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Ok(path) = std::env::join_paths(process::directories()) {
            command.env("PATH", path);
        }
        for (name, sub) in [
            ("TMPDIR", "tmp"),
            ("TEMP", "tmp"),
            ("TMP", "tmp"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("OPENCODE_TEST_HOME", "home"),
            ("OPENCODE_CONFIG_DIR", "config/opencode"),
        ] {
            let path = root.path().join(sub);
            std::fs::create_dir_all(&path).map_err(|_| "Cannot isolate OpenCode directories")?;
            command.env(name, path);
        }
        command
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                "0",
                "--log-level",
                "none",
            ])
            .env("OPENCODE_CONFIG_CONTENT", config.to_string())
            .env("OPENCODE_CONFIG_PROJECT_DISABLE", "1")
            .env("OPENCODE_DISABLE_CLAUDE_CODE", "1")
            .env("OPENCODE_FILEWATCHER_DISABLE", "1")
            .env("OPENCODE_DISABLE_FFF", "1")
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .current_dir(root.path());
        let mut child = Process::spawn(&mut command)?;
        drop(child.input.take());
        let base = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(line) = child
                .lines
                .next_line()
                .await
                .map_err(|_| "Invalid OpenCode startup output")?
            {
                if let Some(base) = listening_url(&line) {
                    return Ok(base);
                }
            }
            Err("OpenCode server did not start")
        })
        .await
        .map_err(|_| "OpenCode startup timeout")??;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "Cannot create OpenCode client")?;
        let server = Self {
            _process: child,
            _root: root,
            client,
            base,
            password,
        };
        // Verify the effective configuration before discovery or inference.
        let entries = server.json(Method::GET, "/api/config", None).await?;
        let documents: Vec<_> = entries
            .as_array()
            .ok_or("Unsupported OpenCode configuration")?
            .iter()
            .filter(|e| e["type"] == "document")
            .collect();
        if documents.len() != 1 || !documents[0]["path"].is_null() || documents[0]["info"] != config
        {
            return Err("OpenCode isolated configuration was not applied".into());
        }
        // model.list waits for plugin settlement; agent.get alone does not.
        server.json(Method::GET, "/api/model", None).await?;
        let plugins = server.json(Method::GET, "/api/plugin", None).await?;
        if !plugins["data"].as_array().is_some_and(|plugins| {
            plugins.iter().all(|plugin| {
                let id = plugin["id"].as_str().unwrap_or("");
                plugin["source"]["type"] == "builtin"
                    && plugin["state"]["status"] == "active"
                    && (id.starts_with("opencode.provider.")
                        || [
                            "opencode.models.dev",
                            "opencode.agent",
                            "opencode.config.agent",
                            "opencode.config.policy",
                        ]
                        .contains(&id))
            })
        }) {
            return Err("OpenCode extension isolation could not be verified".into());
        }
        let agent = server
            .json(Method::GET, "/api/agent/sagaryet", None)
            .await?;
        if !agent["data"]["permissions"]
            .as_array()
            .and_then(|p| p.last())
            .is_some_and(|p| p == &json!({"action":"*","resource":"*","effect":"deny"}))
        {
            return Err("OpenCode tool restrictions could not be verified".into());
        }
        Ok(server)
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .basic_auth("opencode", Some(&self.password))
    }
    async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut request = self.request(method, path).timeout(Duration::from_secs(15));
        if let Some(body) = body {
            request = request.json(body);
        }
        read_json(
            request
                .send()
                .await
                .map_err(|_| "OpenCode connection failed")?,
        )
        .await
    }
}
fn listening_url(line: &str) -> Option<String> {
    let port = line
        .strip_prefix("server listening on http://127.0.0.1:")?
        .trim()
        .parse::<u16>()
        .ok()?;
    (port != 0).then(|| format!("http://127.0.0.1:{port}"))
}
async fn read_json(response: Response) -> Result<Value, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!("OpenCode HTTP {}", status.as_u16()));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "OpenCode response failed")?;
        if bytes.len() + chunk.len() > LIMIT {
            return Err("OpenCode response exceeds limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "Invalid OpenCode response".into())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.chars().any(char::is_whitespace)
        && !id.chars().any(char::is_control)
        && !id.contains('#')
}
pub fn parse_models(value: &Value) -> Result<Vec<Model>, String> {
    let entries = value["data"]
        .as_array()
        .ok_or("Unsupported OpenCode model catalog")?;
    let mut models = Vec::new();
    for entry in entries.iter().take(500) {
        let (Some(provider), Some(id)) = (entry["providerID"].as_str(), entry["id"].as_str())
        else {
            continue;
        };
        if !valid_id(provider)
            || provider.contains('/')
            || !valid_id(id)
            || entry["enabled"] == false
        {
            continue;
        }
        let id = format!("opencode/{provider}/{id}");
        if models.iter().any(|m: &Model| m.id == id) {
            continue;
        }
        let efforts = entry["variants"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|v| v["id"].as_str())
                    .filter(|v| valid_id(v))
                    .take(30)
                    .map(|v| ReasoningOption {
                        reasoning_effort: v.into(),
                        description: String::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        models.push(Model {
            id,
            name: format!(
                "{} · {provider}",
                entry["name"]
                    .as_str()
                    .unwrap_or(entry["id"].as_str().unwrap())
            ),
            provider: "opencode".into(),
            reasoning_efforts: efforts,
            default_effort: None,
            can_serve: true,
            streaming: "incremental",
        });
    }
    Ok(models)
}
pub async fn models() -> Result<Vec<Model>, String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let server = Server::start().await?;
        parse_models(&server.json(Method::GET, "/api/model", None).await?)
    })
    .await
    .map_err(|_| "OpenCode discovery timeout".to_string())?
}
pub async fn credential_listing() -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let server = Server::start().await?;
        // The native equivalent of auth list. Never return its account-bearing body.
        server
            .json(Method::GET, "/api/integration", None)
            .await
            .map(|_| ())
    })
    .await
    .map_err(|_| "OpenCode credential listing timeout".to_string())?
}

#[derive(Default)]
struct Events {
    session: String,
    message: String,
    blocks: HashMap<u64, String>,
    total: usize,
    has_text: bool,
    last_block: Option<u64>,
    stopped: bool,
    done: bool,
}
impl Events {
    fn handle(&mut self, value: &Value) -> Result<Vec<String>, String> {
        let kind = value["type"].as_str().ok_or("Invalid OpenCode event")?;
        let data = &value["data"];
        if matches!(kind, "permission.asked" | "form.created") {
            return Err("Interactive OpenCode action denied".into());
        }
        if data["sessionID"] != self.session {
            return Ok(vec![]);
        }
        if self.done {
            return Err("OpenCode event after completion".into());
        }
        let mut output = vec![];
        match kind {
            "session.step.started" => {
                if !self.message.is_empty() {
                    return Err("Unexpected additional OpenCode step".into());
                }
                self.message = data["assistantMessageID"]
                    .as_str()
                    .ok_or("Missing OpenCode message")?
                    .into();
            }
            "session.text.delta" | "session.text.ended"
                if !self.message.is_empty() && data["assistantMessageID"] == self.message =>
            {
                let ordinal = data["ordinal"]
                    .as_u64()
                    .filter(|n| *n < 2000)
                    .ok_or("Invalid OpenCode text block")?;
                if self.stopped || self.last_block.is_some_and(|last| ordinal < last) {
                    return Err("Out-of-order OpenCode text block".into());
                }
                self.last_block = Some(ordinal);
                let block = self.blocks.entry(ordinal).or_default();
                let text = if kind == "session.text.delta" {
                    data["delta"].as_str().ok_or("Missing OpenCode delta")?
                } else {
                    data["text"]
                        .as_str()
                        .ok_or("Missing OpenCode final text")?
                        .strip_prefix(block.as_str())
                        .ok_or("Inconsistent OpenCode final text")?
                };
                self.has_text |= !text.trim().is_empty();
                self.total += text.len();
                if self.total > 1024 * 1024 {
                    return Err("Answer exceeds output limit".into());
                }
                block.push_str(text);
                if !text.is_empty() {
                    output.push(text.into());
                }
            }
            "session.step.ended" => {
                if self.message.is_empty()
                    || data["assistantMessageID"] != self.message
                    || data["finish"] != "stop"
                {
                    return Err("OpenCode did not finish successfully".into());
                }
                self.stopped = true;
            }
            "session.execution.succeeded" => {
                if !self.stopped || !self.has_text {
                    return Err("OpenCode exited without successful text completion".into());
                }
                self.done = true;
            }
            "session.step.failed" | "session.execution.failed" => {
                return Err(data["error"].to_string());
            }
            _ if kind.starts_with("session.tool.") => return Err("OpenCode tool use denied".into()),
            _ => {}
        }
        Ok(output)
    }
}
#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    data: Vec<u8>,
    bytes: usize,
}
impl Sse {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.bytes += bytes.len();
        if self.bytes > LIMIT {
            return Err("OpenCode stream exceeds limit".into());
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|v| *v == b'\n') {
            let line: Vec<_> = self.pending.drain(..=end).collect();
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(
                        serde_json::from_slice(&self.data)
                            .map_err(|_| "Malformed OpenCode event")?,
                    );
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            }
        }
        Ok(events)
    }
}
pub async fn generate(
    model: &str,
    effort: Option<&str>,
    messages: &[Message],
    tx: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let (provider, model) = model
        .split_once('/')
        .filter(|(p, m)| valid_id(p) && !p.contains('/') && valid_id(m))
        .ok_or("Invalid OpenCode model")?;
    let server = Server::start().await?;
    run_with_server(&server, provider, model, effort, messages, tx).await
}

async fn run_with_server(
    server: &Server,
    provider: &str,
    model: &str,
    effort: Option<&str>,
    messages: &[Message],
    tx: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let mut selected = json!({"providerID":provider,"id":model});
    if let Some(effort) = effort {
        selected["variant"] = json!(effort);
    }
    let session = server.json(Method::POST, "/api/session", Some(&json!({"title":"Sagaryet temporary chat","agent":"sagaryet","model":selected,"permissions":[{"action":"*","resource":"*","effect":"deny"}]}))).await?;
    let id = session["data"]["id"]
        .as_str()
        .filter(|id| {
            id.starts_with("ses")
                && id.len() < 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .ok_or("Invalid OpenCode session ID")?;
    let response = server
        .request(Method::GET, "/api/event")
        .send()
        .await
        .map_err(|_| "OpenCode event connection failed")?;
    if !response.status().is_success()
        || !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        return Err("Invalid OpenCode event stream".into());
    }
    let mut stream = response.bytes_stream();
    let mut sse = Sse::default();
    // Subscribe fully before submitting; the connected frame proves readiness.
    let mut connected = false;
    while !connected {
        let bytes = stream
            .next()
            .await
            .ok_or("OpenCode stream closed")?
            .map_err(|_| "OpenCode stream failed")?;
        connected = sse
            .push(&bytes)?
            .iter()
            .any(|v| v["type"] == "server.connected");
    }
    let text = format!(
        "{}\n{}",
        crate::adapter::instructions(messages),
        crate::adapter::prompt(messages)
    );
    server
        .json(
            Method::POST,
            &format!("/api/session/{id}/prompt"),
            Some(&json!({"text":text})),
        )
        .await?;
    let mut events = Events {
        session: id.into(),
        ..Default::default()
    };
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "OpenCode stream failed")?;
        for value in sse.push(&chunk)? {
            for text in events.handle(&value)? {
                tx.send(Event::Text(text)).await.map_err(|_| "cancelled")?;
            }
            if events.done {
                return Ok(());
            }
        }
    }
    Err("OpenCode stream closed before completion".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(kind: &str, extra: Value) -> Value {
        let mut data = json!({"sessionID":"ses_test","assistantMessageID":"msg_test"});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"type":kind,"data":data})
    }
    fn started() -> Events {
        let mut events = Events {
            session: "ses_test".into(),
            ..Default::default()
        };
        events
            .handle(&event("session.step.started", json!({})))
            .unwrap();
        events
    }
    #[test]
    fn native_models_preserve_provider_and_variants() {
        let models = parse_models(&json!({"data":[
            {"providerID":"one","id":"family/model","name":"Model","variants":[{"id":"high"}]},
            {"providerID":"two","id":"family/model"},
            {"providerID":"one","id":"family/model"},
            {"providerID":"bad/provider","id":"model"},
            {"providerID":"one","id":"hidden","enabled":false}
        ]}))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "opencode/one/family/model");
        assert_eq!(models[0].reasoning_efforts[0].reasoning_effort, "high");
        assert!(parse_models(&json!([])).is_err());
        assert!(supported("opencode v2.0.32"));
        assert!(!supported("1.19.4"));
        assert_eq!(
            listening_url("server listening on http://0.0.0.0:4096"),
            None
        );
    }
    #[test]
    fn incremental_text_excludes_reasoning_and_requires_both_terminal_events() {
        let mut events = started();
        assert!(
            events
                .handle(&event(
                    "session.reasoning.delta",
                    json!({"delta":"private"})
                ))
                .unwrap()
                .is_empty()
        );
        assert!(
            events
                .handle(&event(
                    "session.text.delta",
                    json!({"assistantMessageID":"other","ordinal":0,"delta":"wrong"})
                ))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            events
                .handle(&event(
                    "session.text.delta",
                    json!({"ordinal":0,"delta":"မင်္ဂလာပါ"})
                ))
                .unwrap(),
            vec!["မင်္ဂလာပါ"]
        );
        assert_eq!(
            events
                .handle(&event(
                    "session.text.ended",
                    json!({"ordinal":0,"text":"မင်္ဂလာပါ!"})
                ))
                .unwrap(),
            vec!["!"]
        );
        assert!(!events.done);
        events
            .handle(&event("session.step.ended", json!({"finish":"stop"})))
            .unwrap();
        assert!(!events.done);
        events
            .handle(&event("session.execution.succeeded", json!({})))
            .unwrap();
        assert!(events.done);
    }
    #[test]
    fn failures_tools_truncation_and_conflicting_text_never_succeed() {
        for (kind, data) in [
            ("session.execution.succeeded", json!({})),
            ("session.tool.input.started", json!({"name":"shell"})),
            (
                "session.execution.failed",
                json!({"error":{"message":"quota exhausted"}}),
            ),
            ("session.step.ended", json!({"finish":"length"})),
        ] {
            let mut e = started();
            assert!(e.handle(&event(kind, data)).is_err());
            assert!(!e.done);
        }
        let mut e = started();
        e.handle(&event(
            "session.text.delta",
            json!({"ordinal":0,"delta":"first"}),
        ))
        .unwrap();
        assert!(
            e.handle(&event(
                "session.text.ended",
                json!({"ordinal":0,"text":"different"})
            ))
            .is_err()
        );
    }
    #[test]
    fn sse_handles_split_utf8_and_bounds_all_bytes() {
        let wire = format!(
            "data: {}\r\n\r\n",
            event("session.text.delta", json!({"ordinal":0,"delta":"မ"}))
        );
        let mut sse = Sse::default();
        let mut values = vec![];
        for byte in wire.as_bytes() {
            values.extend(sse.push(&[*byte]).unwrap());
        }
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["data"]["delta"], "မ");
        assert!(Sse::default().push(&vec![b'x'; LIMIT + 1]).is_err());
    }
    #[test]
    fn credential_import_excludes_oauth_and_database_conversations() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("auth.json"), r#"{"api":{"type":"api","key":"legacy"},"oauth":{"type":"oauth","access":"secret"},"remote":{"type":"wellknown","key":"secret"}}"#).unwrap();
        let db = rusqlite::Connection::open(source.path().join("opencode.db")).unwrap();
        db.execute_batch("CREATE TABLE credential (integration_id TEXT, value TEXT, active INTEGER, time_created INTEGER); CREATE TABLE session (prompt TEXT);").unwrap();
        db.execute(
            "INSERT INTO credential VALUES (?1, ?2, 1, 1)",
            ["api", r#"{"type":"key","key":"current"}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO credential VALUES (?1, ?2, 0, 2)",
            ["inactive", r#"{"type":"key","key":"unused"}"#],
        )
        .unwrap();
        let keys = api_keys(source.path()).unwrap();
        assert_eq!(keys, json!({"api":{"type":"api","key":"current"}}));
    }
    #[tokio::test]
    #[ignore = "explicit selected-model synthetic request; consumes provider quota"]
    async fn installed_opencode_connection() {
        let model = std::env::var("SAGARYET_TEST_MODEL")
            .expect("Set SAGARYET_TEST_MODEL to opencode/provider/model");
        assert!(model.starts_with("opencode/"));
        let mut run = crate::adapter::start(
            model,
            None,
            vec![Message {
                role: "user".into(),
                content: "Reply with OK. Do not use tools.".into(),
            }],
        );
        let mut text = false;
        let mut completed = false;
        while let Some(event) = run.events.recv().await {
            match event {
                Event::Text(value) => text |= !value.trim().is_empty(),
                Event::Completed => completed = true,
                Event::Failed(code) => panic!("Selected OpenCode model failed: {code}"),
            }
        }
        assert!(text && completed);
    }
    #[tokio::test]
    #[ignore = "installed OpenCode v2 metadata only; no inference"]
    async fn installed_opencode_catalog() {
        let server = Server::start().await.unwrap();
        let value = server.json(Method::GET, "/api/model", None).await.unwrap();
        let models = parse_models(&value).unwrap();
        println!(
            "OpenCode: {} models; credentials are not a generation verdict",
            models.len()
        );
        for model in &models {
            println!("{}", model.id);
        }
        assert!(!models.is_empty());
        server
            .json(Method::GET, "/api/integration", None)
            .await
            .unwrap();
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        response::{Sse, sse::Event as WireEvent},
        routing::{get, post},
    };
    use std::{convert::Infallible, sync::Arc};
    use tokio::{net::TcpListener, sync::Notify};

    #[derive(Clone)]
    struct Mock {
        mode: &'static str,
        prompt: Arc<Notify>,
    }
    fn event(kind: &str, extra: Value) -> Value {
        let mut data = json!({"sessionID":"ses_mock","assistantMessageID":"msg_mock"});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"type":kind,"data":data})
    }
    async fn mock(mode: &'static str) -> (Server, tokio::task::JoinHandle<()>) {
        let state = Mock {
            mode,
            prompt: Arc::new(Notify::new()),
        };
        let routes = Router::new()
            .route("/api/session", post(|Json(body): Json<Value>| async move {
                assert_eq!(body["model"], json!({"providerID":"provider","id":"model","variant":"high"}));
                assert_eq!(body["agent"], "sagaryet");
                assert_eq!(body["permissions"], json!([{"action":"*","resource":"*","effect":"deny"}]));
                Json(json!({"data":{"id":"ses_mock"}}))
            }))
            .route("/api/session/ses_mock/prompt", post(|State(state): State<Mock>, Json(body): Json<Value>| async move {
                assert!(body["text"].as_str().unwrap().contains("final user message"));
                assert!(body["text"].as_str().unwrap().contains("Synthetic request"));
                state.prompt.notify_one();
                Json(json!({"data":{}}))
            }))
            .route("/api/event", get(|State(state): State<Mock>| async move {
                Sse::new(async_stream::stream! {
                    yield Ok::<_, Infallible>(WireEvent::default().data(json!({"type":"server.connected"}).to_string()));
                    state.prompt.notified().await;
                    for (kind, data) in [
                        ("session.step.started", json!({})),
                        ("session.reasoning.delta", json!({"delta":"never expose this"})),
                        ("session.text.delta", json!({"ordinal":0,"delta":"Hello မ"})),
                    ] { yield Ok(WireEvent::default().data(event(kind, data).to_string())); }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if state.mode == "cancel" { std::future::pending::<()>().await; }
                    let tail = match state.mode {
                        "tool" => vec![("session.tool.input.started", json!({"name":"shell"}))],
                        "failure" => vec![("session.execution.failed", json!({"error":{"message":"quota exhausted"}}))],
                        "truncated" => vec![],
                        _ => vec![
                            ("session.text.delta", json!({"ordinal":0,"delta":"!"})),
                            ("session.text.ended", json!({"ordinal":0,"text":"Hello မ!"})),
                            ("session.step.ended", json!({"finish":"stop"})),
                            ("session.execution.succeeded", json!({})),
                        ],
                    };
                    for (kind, data) in tail { yield Ok(WireEvent::default().data(event(kind, data).to_string())); }
                })
            }))
            .layer(axum::middleware::from_fn(|request: axum::extract::Request, next: axum::middleware::Next| async move {
                assert_eq!(request.headers().get("authorization").unwrap(), "Basic b3BlbmNvZGU6c2VjcmV0");
                next.run(request).await
            }))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let server = Server {
            _process: Process::spawn(&mut command).unwrap(),
            _root: tempfile::tempdir().unwrap(),
            client: Client::builder().no_proxy().build().unwrap(),
            base,
            password: "secret".into(),
        };
        (server, task)
    }
    #[tokio::test]
    async fn native_http_stream_preserves_text_and_rejects_tools_failures_and_eof() {
        for mode in ["success", "tool", "failure", "truncated"] {
            let (server, task) = mock(mode).await;
            let root = server._root.path().to_path_buf();
            let (tx, mut rx) = mpsc::channel(16);
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                run_with_server(
                    &server,
                    "provider",
                    "model",
                    Some("high"),
                    &[Message {
                        role: "user".into(),
                        content: "Synthetic request".into(),
                    }],
                    &tx,
                ),
            )
            .await
            .unwrap();
            drop(tx);
            let mut text = String::new();
            while let Some(event) = rx.recv().await {
                if let Event::Text(value) = event {
                    text.push_str(&value);
                }
            }
            assert_eq!(result.is_ok(), mode == "success");
            assert_eq!(
                text,
                if mode == "success" {
                    "Hello မ!"
                } else {
                    "Hello မ"
                }
            );
            drop(server);
            assert!(
                !root.exists(),
                "Temporary state survived operation completion"
            );
            task.abort();
        }
    }
    #[tokio::test]
    async fn cancelling_native_stream_removes_child_and_temporary_state() {
        let (server, mock_task) = mock("cancel").await;
        let root = server._root.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            run_with_server(
                &server,
                "provider",
                "model",
                Some("high"),
                &[Message {
                    role: "user".into(),
                    content: "Synthetic request".into(),
                }],
                &tx,
            )
            .await
        });
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap(),
            Some(Event::Text(_))
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!root.exists(), "Cancellation left provider state on disk");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .is_none()
        );
        mock_task.abort();
    }
}
