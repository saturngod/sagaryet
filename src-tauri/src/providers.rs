use crate::process;
use serde::Serialize;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub executable: Option<String>,
    pub installed: bool,
    pub runnable: bool,
    pub version: Option<String>,
    pub authentication: String,
    pub credential_listing: &'static str,
    pub generation: &'static str,
    pub message: String,
}
async fn check(id: &str, name: &str) -> Provider {
    let executable = process::executable(id).map(|p| p.to_string_lossy().into_owned());
    let mut result = Provider {
        id: id.into(),
        name: name.into(),
        installed: executable.is_some(),
        executable,
        runnable: false,
        version: None,
        authentication: "not_checked".into(),
        credential_listing: "not_checked",
        generation: "not_tested",
        message: "CLI not found".into(),
    };
    if !result.installed {
        return result;
    }
    match process::capture(id, &["--version"]).await {
        Ok((true, text)) => {
            result.runnable = true;
            result.version = text
                .lines()
                .find(|v| !v.trim().is_empty())
                .map(|v| v.chars().take(160).collect());
        }
        _ => {
            result.message = "CLI found, but version check failed".into();
            return result;
        }
    }
    result.authentication = "unknown".into();
    if id == "codex" {
        if let Ok((true, _)) = process::capture(id, &["login", "status"]).await {
            result.authentication = "reported_authenticated".into();
        }
    } else if id == "claude"
        && let Ok((success, text)) = process::capture(id, &["auth", "status"]).await
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        result.authentication = match (success, value["loggedIn"].as_bool()) {
            (true, Some(true)) => "reported_authenticated",
            (false, Some(false)) => "reported_signed_out",
            _ => "unknown",
        }
        .into();
    }
    if id == "qoder"
        && let Ok((true, text)) = process::capture(id, &["status", "--output", "json"]).await
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        result.authentication = match value["logged_in"].as_bool() {
            Some(true) => "reported_authenticated",
            Some(false) => "reported_signed_out",
            None => "unknown",
        }
        .into();
    }
    if id == "opencode" {
        result.credential_listing = if crate::opencode::credential_listing().await.is_ok() {
            "available"
        } else {
            "failed"
        };
        result.message = "Select a provider/model and test its connection; saved credentials do not prove access".into();
        return result;
    }
    result.message = match result.authentication.as_str() {
        "reported_authenticated" => "Login detected · Live connection not tested",
        "reported_signed_out" => "Sign-in required",
        _ => "CLI runnable · Authentication unknown · Live connection not tested",
    }
    .into();
    result
}
pub async fn list() -> Vec<Provider> {
    let (a, b, c, d) = tokio::join!(
        check("codex", "Codex"),
        check("claude", "Claude Code"),
        check("qoder", "Qoder"),
        check("opencode", "OpenCode")
    );
    vec![a, b, c, d]
}
