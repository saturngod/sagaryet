//! Text-only adapters. All HTTP and explicit live tests share this execution path.
use crate::process::{self, Process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::{io::AsyncWriteExt, sync::mpsc};
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: String,
    pub content: String,
}
#[derive(Debug)]
pub enum Event {
    Text(String),
    Completed,
    Failed(String),
}
pub struct Generation {
    pub events: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn start(model: String, effort: Option<String>, messages: Vec<Message>) -> Generation {
    let (tx, events) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        let execution = async {
            let (provider, model) = model.split_once('/').ok_or("Invalid model")?;
            match provider {
                "codex" => codex(model, effort.as_deref(), &messages, &tx).await,
                "claude" | "qoder" => {
                    structured(provider, model, effort.as_deref(), &messages, &tx).await
                }
                _ => Err(
                    "This CLI does not yet have a verified text-only execution configuration"
                        .into(),
                ),
            }
        };
        let outcome = tokio::select! {
            _ = tx.closed() => return,
            result = tokio::time::timeout(Duration::from_secs(120), execution) => result.unwrap_or_else(|_| Err("timeout".into()))
        };
        let _ = tx
            .send(match outcome {
                Ok(()) => Event::Completed,
                Err(e) => Event::Failed(classify(&e)),
            })
            .await;
    });
    Generation { events, task }
}
pub fn classify(message: &str) -> String {
    let message = message.to_lowercase();
    for (code, needles) in [
        (
            "authentication_required",
            &[
                "authentication",
                "unauthorized",
                "login",
                "log in",
                "sign in",
                "api key",
                "401",
            ][..],
        ),
        (
            "quota_exhausted",
            &["quota", "credit", "rate limit", "usage limit", "429"],
        ),
        ("network_error", &["network", "connection", "fetch failed"]),
        (
            "model_unavailable",
            &["model_not_found", "model not found", "unsupported model"],
        ),
        ("timeout", &["timeout", "timed out"]),
        (
            "unsupported_cli",
            &[
                "configuration",
                "unknown option",
                "unrecognized",
                "not found",
                "tools are enabled",
            ],
        ),
        ("cancelled", &["cancelled"]),
    ] {
        if needles.iter().any(|needle| message.contains(needle)) {
            return code.into();
        }
    }
    "provider_error".into()
}
fn prompt(messages: &[Message]) -> String {
    // JSON escaping prevents accidental delimiters from changing message boundaries.
    serde_json::to_string(
        &messages
            .iter()
            .filter(|m| m.role != "system")
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default()
}
fn instructions(messages: &[Message]) -> String {
    format!(
        "You are a text chat assistant. The input is a JSON array of conversation messages with role and content fields. Continue the conversation by answering the final user message. Do not use tools.\n{}",
        messages
            .first()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .unwrap_or("")
    )
}
async fn emit(tx: &mpsc::Sender<Event>, text: &str, total: &mut usize) -> Result<(), String> {
    *total += text.len();
    if *total > 1024 * 1024 {
        return Err("Answer exceeds output limit".into());
    }
    if !text.is_empty() {
        tx.send(Event::Text(text.into()))
            .await
            .map_err(|_| "cancelled")?;
    }
    Ok(())
}
pub fn structured_args(provider: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--setting-sources",
        "",
        "--no-session-persistence",
        "--settings",
        "{\"disableAllHooks\":true,\"enabledPlugins\":{}}",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if provider == "claude" {
        args.extend(
            [
                "--safe-mode",
                "--disable-slash-commands",
                "--permission-mode",
                "dontAsk",
                "--verbose",
            ]
            .map(String::from),
        );
    } else {
        args.extend(
            [
                "--permission-mode",
                "dont_ask",
                "--max-model-request-retries",
                "0",
            ]
            .map(String::from),
        );
    }
    args
}
async fn structured(
    provider: &str,
    model: &str,
    effort: Option<&str>,
    messages: &[Message],
    tx: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|_| "Cannot create working directory")?;
    let mut command = process::command(provider)?;
    command
        .args(structured_args(provider))
        .args(["--model", model, "--system-prompt", &instructions(messages)])
        .current_dir(dir.path());
    if let Some(effort) = effort {
        command.args([
            if provider == "claude" {
                "--effort"
            } else {
                "--reasoning-effort"
            },
            effort,
        ]);
    }
    if provider == "qoder" {
        let root = dir.path().join("qoder");
        let auth = root.join(".auth");
        std::fs::create_dir_all(&auth).map_err(|_| "Cannot isolate Qoder configuration")?;
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            for name in ["user", "machine_id"] {
                let source = std::path::PathBuf::from(&home)
                    .join(".qoder/.auth")
                    .join(name);
                if source.is_file()
                    && source
                        .metadata()
                        .map_err(|_| "Cannot read Qoder authentication")?
                        .len()
                        <= 128 * 1024
                {
                    std::fs::copy(source, auth.join(name))
                        .map_err(|_| "Cannot load Qoder authentication")?;
                }
            }
        }
        command.arg("--config-dir").arg(root);
    }
    let mut process = Process::spawn(&mut command)?;
    let mut input = process.input.take().ok_or("CLI stdin unavailable")?;
    input
        .write_all(prompt(messages).as_bytes())
        .await
        .map_err(|_| "Cannot write prompt")?;
    drop(input);
    let mut parser = Structured::default();
    while let Some(line) = process
        .lines
        .next_line()
        .await
        .map_err(|_| "Invalid output encoding")?
    {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|_| "Malformed provider JSON")?;
        for text in parser.handle(&value)? {
            emit(tx, &text, &mut parser.bytes).await?;
        }
    }
    let success = process
        .child
        .wait()
        .await
        .map_err(|_| "CLI wait failed")?
        .success();
    if success && parser.initialized && parser.completed && !parser.answer.trim().is_empty() {
        Ok(())
    } else {
        Err("Provider exited without successful text completion".into())
    }
}
#[derive(Default)]
struct Structured {
    initialized: bool,
    answer: String,
    bytes: usize,
    completed: bool,
}
impl Structured {
    fn handle(&mut self, value: &Value) -> Result<Vec<String>, String> {
        let mut output = vec![];
        if value["type"] == "system" && value["subtype"] == "init" {
            self.initialized = value["tools"]
                .as_array()
                .is_some_and(|tools| tools.is_empty());
        }
        if value["type"] == "system"
            && value["subtype"] == "init"
            && value["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        {
            return Err("Provider tools are enabled".into());
        }
        if value["type"] == "control_request" {
            return Err("Interactive request denied".into());
        }
        if value["type"] == "stream_event" {
            let e = &value["event"];
            if e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use" {
                return Err("Tool use denied".into());
            }
            if e["type"] == "content_block_delta"
                && e["delta"]["type"] == "text_delta"
                && value["parent_tool_use_id"].is_null()
                && let Some(text) = e["delta"]["text"].as_str()
            {
                self.answer.push_str(text);
                output.push(text.into());
            }
        }
        if value["type"] == "assistant"
            && value["parent_tool_use_id"].is_null()
            && let Some(blocks) = value["message"]["content"].as_array()
        {
            if blocks.iter().any(|b| b["type"] == "tool_use") {
                return Err("Tool use denied".into());
            }
            let text: String = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            if !text.is_empty() {
                self.reconcile(&text, &mut output)?;
            }
        }
        if value["type"] == "result" {
            if value["subtype"] != "success" || value["is_error"] == true {
                return Err(value.to_string());
            }
            if let Some(text) = value["result"].as_str() {
                self.reconcile(text, &mut output)?;
            }
            self.completed = true;
        }
        Ok(output)
    }
    fn reconcile(&mut self, text: &str, output: &mut Vec<String>) -> Result<(), String> {
        if text == self.answer {
            return Ok(());
        }
        let suffix = text
            .strip_prefix(&self.answer)
            .ok_or("Inconsistent provider final text")?;
        if !suffix.is_empty() {
            output.push(suffix.into());
        }
        self.answer = text.into();
        Ok(())
    }
}
async fn codex(
    model: &str,
    effort: Option<&str>,
    messages: &[Message],
    tx: &mpsc::Sender<Event>,
) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|_| "Cannot isolate Codex")?;
    // Fresh config root excludes inherited hooks, MCP, plugins and project instructions.
    let root = dir.path().join("codex");
    std::fs::create_dir(&root).map_err(|_| "Cannot isolate Codex")?;
    let source = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| process::home_directory().map(|home| home.join(".codex")));
    if let Some(source) = source {
        let auth = source.join("auth.json");
        if auth.is_file() {
            std::fs::copy(auth, root.join("auth.json"))
                .map_err(|_| "Cannot load Codex authentication")?;
        }
    }
    let config = "approval_policy = 'never'\nsandbox_mode = 'read-only'\nweb_search = 'disabled'\nproject_doc_max_bytes = 0\n[features]\nshell_tool = false\nshell_snapshot = false\napply_patch_freeform = false\nremote_plugin = false\napps = false\n[agents]\nenabled = false\n";
    std::fs::write(root.join("config.toml"), config).map_err(|_| "Cannot configure Codex")?;
    let mut command = process::command("codex")?;
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", &root)
        .current_dir(dir.path());
    let mut process = Process::spawn(&mut command)?;
    process.send(&json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"sagaryet","version":"0.1.0"}}})).await?;
    let mut thread = String::new();
    let mut turn = String::new();
    let mut texts = HashMap::<String, String>::new();
    let mut phases = HashMap::<String, String>::new();
    let mut total = 0;
    while let Some(line) = process
        .lines
        .next_line()
        .await
        .map_err(|_| "Invalid Codex output")?
    {
        let v: Value = serde_json::from_str(&line).map_err(|_| "Malformed Codex record")?;
        if v.get("error").is_some() {
            return Err(v["error"].to_string());
        }
        if v.get("method").is_some() && v.get("id").is_some() {
            return Err("Interactive Codex action denied".into());
        }
        match v["id"].as_u64() {
            Some(0) => {
                process.send(&json!({"method":"initialized"})).await?;
                process.send(&json!({"id":1,"method":"thread/start","params":{"model":model,"cwd":dir.path(),"ephemeral":true,"approvalPolicy":"never","sandbox":"read-only","baseInstructions":instructions(messages)}})).await?;
            }
            Some(1) => {
                thread = v["result"]["thread"]["id"]
                    .as_str()
                    .ok_or("Missing thread ID")?
                    .into();
                process.send(&json!({"id":2,"method":"turn/start","params":{"threadId":thread,"model":model,"effort":effort,"input":[{"type":"text","text":prompt(messages)}]}})).await?;
            }
            Some(2) => {
                turn = v["result"]["turn"]["id"]
                    .as_str()
                    .ok_or("Missing turn ID")?
                    .into();
            }
            _ => {}
        }
        let p = &v["params"];
        if v["method"] == "turn/started" && p["threadId"] == thread {
            turn = p["turn"]["id"].as_str().unwrap_or("").into();
        }
        if thread.is_empty() || turn.is_empty() || p["threadId"] != thread {
            continue;
        }
        if v["method"] == "turn/completed" && p["turn"]["id"] == turn {
            return if p["turn"]["status"] == "completed" && total > 0 {
                Ok(())
            } else {
                Err(p["turn"]["error"].to_string())
            };
        }
        if p["turnId"] != turn {
            continue;
        }
        let item = &p["item"];
        if v["method"] == "item/started" {
            if !["agentMessage", "reasoning", "userMessage"]
                .contains(&item["type"].as_str().unwrap_or(""))
            {
                return Err("Non-text Codex tool event denied".into());
            }
            if let (Some(id), Some(phase)) = (item["id"].as_str(), item["phase"].as_str()) {
                phases.insert(id.into(), phase.into());
            }
        }
        if v["method"] == "item/agentMessage/delta" {
            let id = p["itemId"].as_str().ok_or("Missing item ID")?;
            let text = p["delta"].as_str().ok_or("Missing text delta")?;
            texts.entry(id.into()).or_default().push_str(text);
            if phases.get(id).is_some_and(|phase| phase == "final_answer") {
                emit(tx, text, &mut total).await?;
            }
        }
        if v["method"] == "item/completed" && item["type"] == "agentMessage" {
            let id = item["id"].as_str().ok_or("Missing item ID")?;
            let phase = item["phase"]
                .as_str()
                .or_else(|| phases.get(id).map(String::as_str));
            if phase == Some("final_answer") || phase.is_none() {
                let text = item["text"].as_str().ok_or("Missing final text")?;
                let prior = if phases.get(id).is_some_and(|p| p == "final_answer") {
                    texts.get(id).map(String::as_str).unwrap_or("")
                } else {
                    ""
                };
                emit(
                    tx,
                    text.strip_prefix(prior)
                        .ok_or("Inconsistent final answer")?,
                    &mut total,
                )
                .await?;
            }
        }
        if v["method"] == "error" && p["willRetry"] != true {
            return Err(p.to_string());
        }
    }
    Err("Codex exited before completion".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_providers_request_partial_messages() {
        for provider in ["claude", "qoder"] {
            let args = structured_args(provider);
            assert_eq!(
                args.iter()
                    .filter(|arg| *arg == "--include-partial-messages")
                    .count(),
                1
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--output-format", "stream-json"])
            );
        }
    }
    #[test]
    fn reconciles_deltas_excludes_thinking_and_requires_success() {
        let mut p = Structured::default();
        assert!(p.handle(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"secret"}}})).unwrap().is_empty());
        assert_eq!(p.handle(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"မင်္ဂလာပါ"}}})).unwrap(), vec!["မင်္ဂလာပါ"]);
        assert!(
            p.handle(&json!({"type":"result","subtype":"success","result":"မင်္ဂလာပါ"}))
                .unwrap()
                .is_empty()
        );
        assert!(p.completed);
        assert!(
            p.handle(&json!({"type":"result","subtype":"error","is_error":true,"error":"quota"}))
                .is_err()
        );
    }
    #[test]
    fn rejects_tools_and_conflicting_final_text() {
        let mut p = Structured::default();
        assert!(
            p.handle(&json!({"type":"system","subtype":"init","tools":["Bash"]}))
                .is_err()
        );
        p.answer = "first".into();
        assert!(
            p.handle(&json!({"type":"result","subtype":"success","result":"different"}))
                .is_err()
        );
    }
    #[tokio::test]
    #[ignore = "explicit live generation; consumes quota"]
    async fn installed_live_connection() {
        let model = std::env::var("SAGARYET_TEST_MODEL").unwrap();
        let mut generation = start(
            model,
            None,
            vec![Message {
                role: "user".into(),
                content:
                    "Write a numbered list of 20 short facts about the ocean. Do not use tools."
                        .into(),
            }],
        );
        let started = std::time::Instant::now();
        let mut deltas = 0;
        let mut text = false;
        let mut completed = false;
        let mut first_text = None;
        while let Some(event) = generation.events.recv().await {
            match event {
                Event::Text(t) => {
                    first_text.get_or_insert_with(|| started.elapsed());
                    text |= !t.trim().is_empty();
                    deltas += 1;
                }
                Event::Completed => completed = true,
                Event::Failed(code) => panic!("Live check failed: {code}"),
            }
        }
        println!(
            "Answer events: {deltas}; first_text_ms={}; elapsed_ms={}",
            first_text.unwrap_or_default().as_millis(),
            started.elapsed().as_millis()
        );
        assert!(text && completed);
        assert!(
            deltas > 1,
            "Expected incremental text, got {deltas} event(s)"
        );
    }
}
