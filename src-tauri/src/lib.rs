mod adapter;
mod catalog;
mod process;
mod providers;
mod server;
use server::{Controller, Status};

#[tauri::command]
async fn server_status(state: tauri::State<'_, Controller>) -> Result<Status, String> {
    Ok(state.status().await)
}
#[tauri::command]
async fn start_server(
    state: tauri::State<'_, Controller>,
    port: u16,
    access_key: Option<String>,
    provider: String,
    reasoning_effort: Option<String>,
) -> Result<Status, String> {
    state
        .start_network(port, access_key, &provider, reasoning_effort)
        .await
}
#[tauri::command]
async fn list_models() -> catalog::Catalog {
    catalog::list().await
}
#[tauri::command]
async fn list_providers() -> Vec<providers::Provider> {
    providers::list().await
}

#[tauri::command]
async fn test_connection(
    model: String,
    reasoning_effort: Option<String>,
) -> Result<serde_json::Value, String> {
    let catalog = catalog::list().await;
    let selected = catalog
        .models
        .iter()
        .find(|m| m.id == model && m.can_serve)
        .ok_or("Select an available model")?;
    if reasoning_effort.as_ref().is_some_and(|effort| {
        !selected
            .reasoning_efforts
            .iter()
            .any(|e| &e.reasoning_effort == effort)
    }) {
        return Err("Unsupported reasoning effort".into());
    }
    let started = std::time::Instant::now();
    let mut run = adapter::start(
        model.clone(),
        reasoning_effort,
        vec![adapter::Message {
            role: "user".into(),
            content: "Reply with OK. Do not use tools.".into(),
        }],
    );
    let mut received = false;
    while let Some(event) = run.events.recv().await {
        match event {
            adapter::Event::Text(text) => received |= !text.trim().is_empty(),
            adapter::Event::Completed if received => {
                return Ok(
                    serde_json::json!({"model":model,"ready":true,"elapsedMs":started.elapsed().as_millis(),"testedAt":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()}),
                );
            }
            adapter::Event::Failed(code) => return Err(code),
            _ => {}
        }
    }
    Err("Provider did not complete a text response".into())
}
#[tauri::command]
fn generate_api_key() -> String {
    server::generate_key()
}
#[tauri::command]
async fn stop_server(state: tauri::State<'_, Controller>) -> Result<Status, String> {
    Ok(state.stop().await)
}
#[tauri::command]
async fn cancel_requests(state: tauri::State<'_, Controller>) -> Result<(), String> {
    state.cancel().await;
    Ok(())
}

#[tauri::command]
async fn rotate_key(state: tauri::State<'_, Controller>) -> Result<Status, String> {
    state.rotate().await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Controller::default())
        .invoke_handler(tauri::generate_handler![
            server_status,
            start_server,
            stop_server,
            rotate_key,
            list_models,
            list_providers,
            test_connection,
            cancel_requests,
            generate_api_key
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sagaryet");
}
