//! Discover metadata only. No threads or inference turns are created here.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningOption {
    pub reasoning_effort: String,
    pub description: String,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub reasoning_efforts: Vec<ReasoningOption>,
    pub default_effort: Option<String>,
    pub can_serve: bool,
    pub streaming: &'static str,
}
#[derive(Serialize)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub warnings: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexModel {
    model: String,
    display_name: String,
    #[serde(default)]
    supported_reasoning_efforts: Vec<ReasoningOption>,
    default_reasoning_effort: Option<String>,
    #[serde(default)]
    hidden: bool,
}
fn parse_models(value: &Value) -> Result<Vec<Model>, String> {
    let entries: Vec<CodexModel> = serde_json::from_value(value["data"].clone())
        .map_err(|_| "Codex returned an unsupported model catalog format".to_string())?;
    Ok(entries
        .into_iter()
        .filter(|m| !m.hidden)
        .map(|m| Model {
            id: format!("codex/{}", m.model),
            name: m.display_name,
            provider: "codex".into(),
            default_effort: m.default_reasoning_effort.filter(|default| {
                m.supported_reasoning_efforts
                    .iter()
                    .any(|r| &r.reasoning_effort == default)
            }),
            reasoning_efforts: m.supported_reasoning_efforts,
            can_serve: true,
            streaming: "incremental",
        })
        .collect())
}
async fn send(pipe: &mut ChildStdin, value: Value) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec(&value).map_err(|_| "Cannot encode model discovery request")?;
    bytes.push(b'\n');
    pipe.write_all(&bytes)
        .await
        .map_err(|_| "Cannot write to Codex App Server".to_string())
}
async fn exchange(child: &mut Child) -> Result<Vec<Model>, String> {
    let mut input = child.stdin.take().ok_or("Codex stdin unavailable")?;
    let output = child.stdout.take().ok_or("Codex stdout unavailable")?;
    // Bound the whole exchange, including a malicious unterminated record.
    let mut lines = BufReader::new(output.take(1024 * 1024)).lines();
    send(&mut input,json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"sagaryet","version":"0.1.0"}}})).await?;
    let mut request_id = 0u64;
    let mut models = vec![];
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|_| "Cannot read Codex model catalog")?
    {
        let event: Value =
            serde_json::from_str(&line).map_err(|_| "Codex returned malformed protocol data")?;
        if event.get("method").is_some() && event.get("id").is_some() {
            return Err("Codex requested an interactive action during model discovery".into());
        }
        if event.get("id").and_then(Value::as_u64) != Some(request_id) {
            continue;
        }
        if event.get("error").is_some() {
            return Err("Codex rejected model discovery; check its login and version".into());
        }
        if request_id == 0 {
            send(&mut input, json!({"method":"initialized"})).await?;
        } else {
            models.extend(parse_models(&event["result"])?);
            if event["result"]["nextCursor"].is_null() {
                return Ok(models);
            }
            if request_id >= 20 {
                return Err("Codex model catalog exceeded the page limit".into());
            }
        }
        let cursor = if request_id == 0 {
            Value::Null
        } else {
            event["result"]["nextCursor"].clone()
        };
        request_id += 1;
        send(&mut input,json!({"id":request_id,"method":"model/list","params":{"limit":100,"includeHidden":false,"cursor":cursor}})).await?;
    }
    Err("Codex closed model discovery or exceeded the output limit".into())
}
async fn codex_models() -> Result<Vec<Model>, String> {
    let cwd = tempfile::tempdir().map_err(|_| "Cannot create temporary discovery directory")?;
    let mut command = crate::process::command("codex")?;
    command
        .arg("app-server")
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|_| "Codex CLI was not found on PATH or could not be started")?;
    let result = tokio::time::timeout(Duration::from_secs(20), exchange(&mut child))
        .await
        .map_err(|_| "Codex model discovery timed out".to_string())
        .and_then(|result| result);
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The child owns a fresh process group; metadata helpers must not outlive it.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}
fn model(provider: &str, id: &str, name: &str, efforts: Vec<String>) -> Model {
    Model {
        id: format!("{provider}/{id}"),
        name: name.into(),
        provider: provider.into(),
        reasoning_efforts: efforts
            .into_iter()
            .map(|reasoning_effort| ReasoningOption {
                reasoning_effort,
                description: String::new(),
            })
            .collect(),
        default_effort: None,
        can_serve: true,
        streaming: if matches!(provider, "claude" | "qoder") {
            "incremental"
        } else {
            "buffered"
        },
    }
}
async fn structured_models(provider: &str) -> Result<Vec<Model>, String> {
    let dir = tempfile::tempdir().map_err(|_| "Cannot create discovery directory")?;
    let mut command = crate::process::command(provider)?;
    command
        .args(crate::adapter::structured_args(provider))
        .args(["--input-format", "stream-json"])
        .current_dir(dir.path());
    let mut process = crate::process::Process::spawn(&mut command)?;
    let request = if provider == "claude" {
        json!({"subtype":"initialize"})
    } else {
        json!({"subtype":"get_models","fetchStrategy":"live"})
    };
    process
        .send(&json!({"type":"control_request","request_id":"sagaryet-models","request":request}))
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = process
            .lines
            .next_line()
            .await
            .map_err(|_| "Invalid discovery output")?
        {
            let v: Value = serde_json::from_str(&line).map_err(|_| "Invalid discovery JSON")?;
            if v["type"] != "control_response" || v["response"]["request_id"] != "sagaryet-models" {
                continue;
            }
            if v["response"]["subtype"] != "success" {
                return Err("CLI rejected model discovery".into());
            }
            let body = &v["response"]["response"];
            let entries = body["models"]
                .as_array()
                .or_else(|| body["supportedModels"].as_array())
                .ok_or("Model catalog not available")?;
            let mut models = vec![];
            for entry in entries.iter().take(500) {
                if entry["isEnabled"] == false {
                    continue;
                }
                let Some(id) = entry["value"].as_str().or_else(|| entry["id"].as_str()) else {
                    continue;
                };
                if id.is_empty() || id.len() > 200 || id.chars().any(char::is_control) {
                    continue;
                }
                let name = entry["displayName"]
                    .as_str()
                    .or_else(|| entry["display_name"].as_str())
                    .unwrap_or(id);
                let efforts = entry["efforts"]
                    .as_array()
                    .or_else(|| entry["supportedEffortLevels"].as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut discovered = model(provider, id, name, efforts);
                discovered.default_effort = entry
                    .pointer("/thinking_config/enabled/efforts")
                    .and_then(Value::as_object)
                    .and_then(|entries| {
                        entries
                            .iter()
                            .find(|(_, v)| v["is_default"] == true)
                            .map(|(k, _)| k.clone())
                    });
                models.push(discovered);
            }
            return Ok(models);
        }
        Err("CLI closed model discovery".into())
    })
    .await
    .map_err(|_| "Model discovery timed out".to_string())?
}
pub async fn list() -> Catalog {
    let (codex, claude, qoder) = tokio::join!(
        codex_models(),
        structured_models("claude"),
        structured_models("qoder")
    );
    let mut catalog = Catalog {
        models: vec![],
        warnings: vec![],
    };
    for (provider, result) in [("Codex", codex), ("Claude Code", claude), ("Qoder", qoder)] {
        match result {
            Ok(models) => catalog.models.extend(models),
            Err(message) => catalog.warnings.push(format!("{provider}: {message}")),
        }
    }
    catalog
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_efforts_come_from_catalog_and_hidden_models_are_filtered() {
        let models=parse_models(&json!({"data":[
            {"model":"one","displayName":"One","defaultReasoningEffort":"high","supportedReasoningEfforts":[{"reasoningEffort":"high","description":"Deep"}]},
            {"model":"two","displayName":"Two","defaultReasoningEffort":"unknown","supportedReasoningEfforts":[]},
            {"model":"hidden","displayName":"Hidden","hidden":true}
        ]})).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "codex/one");
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(models[0].reasoning_efforts.len(), 1);
        assert_eq!(models[1].default_effort, None);
        assert!(models[0].can_serve);
    }
    #[tokio::test]
    #[ignore = "installed CLI discovery; no inference"]
    async fn installed_all_catalogs() {
        let providers = crate::providers::list().await;
        for p in providers {
            println!(
                "{}: installed={} runnable={} auth={}",
                p.id, p.installed, p.runnable, p.authentication
            );
        }
        let catalog = list().await;
        for provider in ["codex", "claude", "qoder"] {
            println!(
                "{provider}: {} models; first={}",
                catalog
                    .models
                    .iter()
                    .filter(|m| m.provider == provider)
                    .count(),
                catalog
                    .models
                    .iter()
                    .find(|m| m.provider == provider)
                    .map(|m| m.id.as_str())
                    .unwrap_or("none")
            );
        }
        for warning in catalog.warnings {
            println!("Warning: {warning}");
        }
        assert!(!catalog.models.is_empty());
    }
    #[tokio::test]
    #[ignore = "requires an installed Codex CLI; metadata only"]
    async fn installed_codex_catalog() {
        let models = codex_models().await.unwrap();
        assert!(!models.is_empty());
        println!("Discovered {} Codex models", models.len());
    }
}
