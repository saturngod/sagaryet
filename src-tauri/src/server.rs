//! Authenticated text-chat transport with bounded admission and adapter cancellation.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use if_addrs::{IfAddr, get_if_addrs};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore, watch},
    task::JoinHandle,
};

const DEFAULT_PORT: u16 = 9693;

async fn bind_available_port(start: u16) -> Result<TcpListener, String> {
    let start = if start == 0 { DEFAULT_PORT } else { start };
    for port in start..=u16::MAX {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(format!("Cannot bind port {port}: {error}")),
        }
    }
    Err(format!("No available port between {start} and 65535"))
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub running: bool,
    pub base_url: Option<String>,
    pub access_key: Option<String>,
    pub provider: String,
    pub model: String,
    pub lan_urls: Vec<String>,
    pub queued: usize,
    pub last_result: Option<String>,
    pub reasoning_effort: Option<String>,
    pub active: usize,
    pub admitted: u64,
}
struct Running {
    state: Arc<ServerState>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    port: u16,
}
#[derive(Default)]
pub struct Controller(Mutex<Option<Running>>);
struct ServerState {
    key: RwLock<String>,
    slots: Arc<Semaphore>,
    admitted: AtomicU64,
    stop: watch::Receiver<bool>,
    model: String,
    models: Vec<crate::catalog::Model>,
    reasoning: Option<String>,
    admission: Arc<Semaphore>,
    cancel: watch::Sender<u64>,
    last_result: std::sync::Mutex<Option<String>>,
    port: u16,
    rate: std::sync::Mutex<(std::time::Instant, u32)>,
}
pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
fn lan_ipv4_addrs() -> Vec<Ipv4Addr> {
    let mut addresses: Vec<Ipv4Addr> = get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback() && !interface.is_link_local())
        .filter_map(|interface| match interface.addr {
            IfAddr::V4(address) => Some(address.ip),
            IfAddr::V6(_) => None,
        })
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}
fn lan_urls(port: u16) -> Vec<String> {
    lan_ipv4_addrs()
        .into_iter()
        .map(|ip| format!("http://{ip}:{port}/v1"))
        .collect()
}
fn host_allowed(host: &str, port: u16) -> bool {
    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    if host_port.parse::<u16>() != Ok(port) {
        return false;
    }
    if name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" {
        return true;
    }
    name.parse::<Ipv4Addr>()
        .is_ok_and(|ip| lan_ipv4_addrs().contains(&ip))
}
fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error":{"message":message,"type":if status.is_server_error() { "provider_error" } else { "invalid_request_error" },"code":code,"param":null}}))).into_response()
}
impl Controller {
    #[cfg(test)]
    pub async fn start(&self, port: u16) -> Result<Status, String> {
        self.start_configured(port, None, "synthetic/test", None)
            .await
    }
    #[cfg(test)]
    pub async fn start_configured(
        &self,
        port: u16,
        access_key: Option<String>,
        model: &str,
        reasoning: Option<String>,
    ) -> Result<Status, String> {
        self.start_network(port, access_key, model, reasoning).await
    }
    pub async fn start_network(
        &self,
        port: u16,
        access_key: Option<String>,
        model: &str,
        reasoning: Option<String>,
    ) -> Result<Status, String> {
        if model == "synthetic/test" && !cfg!(test) {
            return Err("Synthetic models are internal test fixtures".into());
        }
        let models = if model == "synthetic/test" {
            vec![]
        } else {
            let provider = model.split('/').next().unwrap_or(model);
            crate::catalog::list()
                .await
                .models
                .into_iter()
                .filter(|m| m.provider == provider && m.can_serve)
                .collect::<Vec<_>>()
        };
        if model != "synthetic/test" && model.contains('/') && !models.iter().any(|m| m.id == model)
        {
            return Err("Model is unavailable; refresh the model list".into());
        }
        let model = if model == "synthetic/test" {
            model
        } else {
            models
                .iter()
                .find(|m| m.id == model)
                .or_else(|| models.first())
                .ok_or("No available models; refresh the model list")?
                .id
                .as_str()
        };
        if model == "synthetic/test" && reasoning.is_some() {
            return Err("The synthetic model does not support reasoning effort".into());
        }
        let access_key = access_key.unwrap_or_else(generate_key);
        if access_key.len() > 512 || !access_key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(
                "Use an empty API key or up to 512 visible ASCII characters without spaces".into(),
            );
        }
        let mut guard = self.0.lock().await;
        if guard.is_some() {
            return Err("Server is already running".into());
        }
        // Keep the successful listener so another process cannot claim the port.
        let listener = bind_available_port(port).await?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (stop, receiver) = watch::channel(false);
        let state = Arc::new(ServerState {
            key: RwLock::new(access_key),
            slots: Arc::new(Semaphore::new(1)),
            admitted: AtomicU64::new(0),
            stop: receiver.clone(),
            model: model.into(),
            models: models.clone(),
            reasoning,
            admission: Arc::new(Semaphore::new(5)),
            cancel: watch::channel(0).0,
            last_result: std::sync::Mutex::new(None),
            port,
            rate: std::sync::Mutex::new((std::time::Instant::now(), 0)),
        });
        let app = router(state.clone());
        let mut shutdown = receiver;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await;
        });
        *guard = Some(Running {
            state,
            stop,
            task,
            port,
        });
        Ok(status_of(guard.as_ref()).await)
    }
    pub async fn status(&self) -> Status {
        status_of(self.0.lock().await.as_ref()).await
    }
    pub async fn rotate(&self) -> Result<Status, String> {
        let guard = self.0.lock().await;
        let running = guard.as_ref().ok_or("Start the server first")?;
        *running.state.key.write().await = generate_key();
        Ok(status_of(Some(running)).await)
    }
    pub async fn cancel(&self) {
        if let Some(running) = self.0.lock().await.as_ref() {
            running.state.cancel.send_modify(|v| *v += 1);
        }
    }
    pub async fn stop(&self) -> Status {
        let mut guard = self.0.lock().await;
        if let Some(mut running) = guard.take() {
            let _ = running.stop.send(true);
            if tokio::time::timeout(Duration::from_secs(2), &mut running.task)
                .await
                .is_err()
            {
                running.task.abort();
                let _ = (&mut running.task).await;
            }
        }
        status_of(None).await
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}
async fn status_of(running: Option<&Running>) -> Status {
    match running {
        Some(r) => Status {
            running: !r.task.is_finished(),
            base_url: Some(format!("http://127.0.0.1:{}/v1", r.port)),
            access_key: Some(r.state.key.read().await.clone()),
            provider: r.state.model.split('/').next().unwrap_or("codex").into(),
            model: r.state.model.clone(),
            lan_urls: lan_urls(r.port),
            queued: (5 - r.state.admission.available_permits())
                .saturating_sub(1 - r.state.slots.available_permits()),
            last_result: r.state.last_result.lock().unwrap().clone(),
            reasoning_effort: r.state.reasoning.clone(),
            active: 1 - r.state.slots.available_permits(),
            admitted: r.state.admitted.load(Ordering::Relaxed),
        },
        None => Status {
            running: false,
            base_url: None,
            access_key: None,
            provider: "codex".into(),
            model: String::new(),
            lan_urls: vec![],
            queued: 0,
            last_result: None,
            reasoning_effort: None,
            active: 0,
            admitted: 0,
        },
    }
}
fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "Unknown endpoint") })
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}
async fn authorize(State(state): State<Arc<ServerState>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, state.port) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_host",
            "Host must match the local server address",
        );
    }
    if req.headers().contains_key("origin") {
        return error(
            StatusCode::FORBIDDEN,
            "browser_access_disabled",
            "Browser origins are disabled",
        );
    }
    if req.uri().path() != "/healthz" {
        let supplied = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let expected = state.key.read().await;
        if !expected.is_empty()
            && (supplied.is_empty() || !bool::from(supplied.as_bytes().ct_eq(expected.as_bytes())))
        {
            return error(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "A valid bearer access key is required",
            );
        }
    }
    if req.uri().path() != "/healthz" {
        let limited = {
            let mut rate = state.rate.lock().unwrap();
            if rate.0.elapsed() >= Duration::from_secs(60) {
                *rate = (std::time::Instant::now(), 0);
            }
            rate.1 += 1;
            rate.1 > 60
        };
        if limited {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "At most 60 authenticated requests per minute are allowed",
            );
        }
    }
    next.run(req).await
}
async fn models(State(state): State<Arc<ServerState>>) -> Json<Value> {
    let data: Vec<Value> = if state.model == "synthetic/test" {
        vec![json!({"id":state.model,"object":"model","created":0,"owned_by":"synthetic"})]
    } else {
        state.models.iter().map(|model| json!({"id":model.id,"object":"model","created":0,"owned_by":model.provider})).collect()
    };
    Json(json!({"object":"list","data":data}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    model: String,
    messages: Vec<Message>,
    #[serde(default)]
    stream: bool,
    n: Option<u8>,
    reasoning_effort: Option<String>,
}
use crate::adapter::Message;
fn validate(chat: &Chat) -> Result<(), &'static str> {
    if chat.n.is_some_and(|n| n != 1) {
        return Err("Only n=1 is supported");
    }
    if chat.messages.is_empty() || chat.messages.len() > 128 {
        return Err("Supply 1 to 128 text messages");
    }
    let mut expected = "user";
    for (index, message) in chat.messages.iter().enumerate() {
        if message.content.trim().is_empty() {
            return Err("Message content must not be empty");
        }
        if index == 0 && message.role == "system" {
            continue;
        }
        if message.role != expected {
            return Err(
                "Use an optional initial system message, then alternating user and assistant messages",
            );
        }
        expected = if expected == "user" {
            "assistant"
        } else {
            "user"
        };
    }
    if chat.messages.last().map(|m| m.role.as_str()) != Some("user") {
        return Err("The last message must be from the user");
    }
    Ok(())
}
async fn chat(
    State(state): State<Arc<ServerState>>,
    input: Result<Json<Chat>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(chat) = match input {
        Ok(value) => value,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Expected text-only Chat Completions JSON; unknown fields and unsupported features are rejected",
            );
        }
    };
    if let Err(message) = validate(&chat) {
        return error(StatusCode::BAD_REQUEST, "invalid_request", message);
    }
    let requested_model = &chat.model;
    let metadata = state
        .models
        .iter()
        .find(|model| model.id == *requested_model);
    if metadata.is_none()
        && !(state.model == "synthetic/test" && requested_model == "synthetic/test")
    {
        return error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "Unknown model for the active adapter",
        );
    }
    if chat.reasoning_effort.as_ref().is_some_and(|r| {
        metadata.map_or(Some(r) != state.reasoning.as_ref(), |model| {
            !model
                .reasoning_efforts
                .iter()
                .any(|option| option.reasoning_effort == *r)
        })
    }) {
        return error(
            StatusCode::BAD_REQUEST,
            "unsupported_reasoning_effort",
            "Unsupported reasoning effort for this model",
        );
    }
    let admission = match state.admission.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return error(
                StatusCode::TOO_MANY_REQUESTS,
                "queue_full",
                "One active request and four waiting requests are allowed",
            );
        }
    };
    let mut stop = state.stop.clone();
    let mut cancelled = state.cancel.subscribe();
    let permit = tokio::select! {
        _ = stop.changed() => return error(StatusCode::SERVICE_UNAVAILABLE,"server_stopping","Server stopping"),
        _ = cancelled.changed() => return error(StatusCode::SERVICE_UNAVAILABLE,"cancelled","Request cancelled"),
        result = tokio::time::timeout(Duration::from_secs(30),state.slots.clone().acquire_owned()) => match result {
            Ok(Ok(permit)) => permit,
            _ => return error(StatusCode::TOO_MANY_REQUESTS,"queue_timeout","Queue wait exceeded 30 seconds"),
        }
    };
    state.admitted.fetch_add(1, Ordering::Relaxed);
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let id = format!("chatcmpl-{}", generate_key());
    let model = requested_model.clone();
    let selected_model = model.clone();
    let effort = chat
        .reasoning_effort
        .clone()
        .or_else(|| state.reasoning.clone());
    let response = async_stream::stream! {
        let _admission = admission;
        let _permit = permit;
        if selected_model == "synthetic/test" {
            for text in ["Synthetic test response. ", "မင်္ဂလာပါ။ ", "No AI provider was contacted."] {
                tokio::select! {
                    _ = stop.changed() => { yield crate::adapter::Event::Failed("cancelled".into()); return; }
                    _ = cancelled.changed() => { yield crate::adapter::Event::Failed("cancelled".into()); return; }
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
                yield crate::adapter::Event::Text(text.into());
            }
            yield crate::adapter::Event::Completed;
        } else {
            let mut run = crate::adapter::start(selected_model, effort, chat.messages);
            loop {
                tokio::select! {
                    _ = stop.changed() => { yield crate::adapter::Event::Failed("cancelled".into()); return; }
                    _ = cancelled.changed() => { yield crate::adapter::Event::Failed("cancelled".into()); return; }
                    event = run.events.recv() => match event { Some(event) => yield event, None => return }
                }
            }
        }
    };
    use futures_util::{StreamExt, pin_mut};
    if !chat.stream {
        pin_mut!(response);
        let mut answer = String::new();
        while let Some(event) = response.next().await {
            match event {
                crate::adapter::Event::Text(text) => answer.push_str(&text),
                crate::adapter::Event::Failed(code) => {
                    *state.last_result.lock().unwrap() = Some(code.clone());
                    return error(
                        StatusCode::BAD_GATEWAY,
                        &code,
                        "Provider request failed; check login, quota and connection",
                    );
                }
                crate::adapter::Event::Completed => {
                    *state.last_result.lock().unwrap() = Some("Completed".into());
                    return Json(json!({"id":id,"object":"chat.completion","created":created,"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}]})).into_response();
                }
            }
        }
        return error(
            StatusCode::BAD_GATEWAY,
            "provider_error",
            "Provider closed without completion",
        );
    }
    let stream = async_stream::stream! {
        pin_mut!(response);
        let chunk = |delta: Value, finish: Value| json!({"id":id,"object":"chat.completion.chunk","created":created,
            "model":model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string();
        yield Ok::<_, Infallible>(Event::default().data(chunk(json!({"role":"assistant"}),Value::Null)));
        while let Some(event) = response.next().await {
            match event {
                crate::adapter::Event::Text(text) => yield Ok(Event::default().data(chunk(json!({"content":text}),Value::Null))),
                crate::adapter::Event::Completed => {
                    *state.last_result.lock().unwrap() = Some("Completed".into());
                    yield Ok(Event::default().data(chunk(json!({}),json!("stop"))));
                    yield Ok(Event::default().data("[DONE]")); return;
                }
                crate::adapter::Event::Failed(code) => {
                    *state.last_result.lock().unwrap() = Some(code.clone());
                    yield Ok(Event::default().data(json!({"error":{"code":code,"message":"Provider request failed","type":"provider_error"}}).to_string())); return;
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Exercise real sockets without installed provider CLIs on every CI platform.
    #[tokio::test]
    async fn health_and_listener_lifecycle() {
        let controller = Controller::default();
        let status = controller.start(0).await.unwrap();
        let base = status.base_url.unwrap();
        let health = format!("{}/healthz", base.strip_suffix("/v1").unwrap());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client.get(&health).send().await.unwrap().status(),
            StatusCode::OK
        );
        assert!(controller.start(0).await.is_err());
        assert!(!controller.stop().await.running);
        assert!(client.get(&health).send().await.is_err());
        assert!(controller.start(0).await.unwrap().running);
        controller.stop().await;
    }

    #[tokio::test]
    async fn occupied_ports_are_tried_in_order() {
        let first = bind_available_port(0).await.unwrap();
        let port = first.local_addr().unwrap().port();
        assert!(port >= DEFAULT_PORT);
        let second = bind_available_port(port).await.unwrap();
        let next = second.local_addr().unwrap().port();
        assert!(next > port);
        for skipped in port + 1..next {
            assert!(TcpListener::bind(("0.0.0.0", skipped)).await.is_err());
        }
        drop(first);
        let reused = bind_available_port(port).await.unwrap();
        assert_eq!(reused.local_addr().unwrap().port(), port);
    }

    #[test]
    fn host_header_allows_only_local_addresses() {
        assert!(host_allowed("127.0.0.1:4321", 4321));
        assert!(host_allowed("LocalHost:4321", 4321));
        assert!(!host_allowed("127.0.0.1:4322", 4321));
        assert!(!host_allowed("attacker.example:4321", 4321));
        let urls = lan_urls(4321);
        assert_eq!(urls.len(), lan_ipv4_addrs().len());
        for ip in lan_ipv4_addrs() {
            assert!(host_allowed(&format!("{ip}:4321"), 4321));
            assert!(urls.contains(&format!("http://{ip}:4321/v1")));
        }
    }
    #[tokio::test]
    #[ignore = "requires a reachable local network interface"]
    async fn lan_address_serves_models() {
        let controller = Controller::default();
        let status = controller.start(0).await.unwrap();
        let key = status.access_key.clone().unwrap();
        let urls = status.lan_urls.clone();
        assert!(!urls.is_empty(), "no local network address found");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut reached = None;
        for url in &urls {
            if let Ok(response) = client
                .get(format!("{url}/models"))
                .bearer_auth(&key)
                .send()
                .await
                && response.status() == StatusCode::OK
            {
                reached = Some(url.clone());
                break;
            }
        }
        println!("reachable local network URL: {reached:?}");
        assert!(reached.is_some(), "no local network URL was reachable");
        controller.stop().await;
    }
    #[tokio::test]
    async fn authenticated_http_and_stream_contract() {
        let controller = Controller::default();
        let status = controller.start(0).await.unwrap();
        let base = status.base_url.unwrap();
        let key = status.access_key.unwrap();
        let client = reqwest::Client::new();
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let models: Value = client
            .get(format!("{base}/models"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"].as_array().unwrap().len(), 1);
        assert_eq!(models["data"][0]["id"], "synthetic/test");
        let input =
            json!({"model":"synthetic/test","messages":[{"role":"user","content":"hello"}]});
        let buffered: Value = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&input)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut streaming = input.clone();
        streaming["stream"] = json!(true);
        let response = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&streaming)
            .send()
            .await
            .unwrap();
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        // Consume the body as it arrives: collecting response.text() alone would
        // also pass if the server buffered the entire generation.
        let mut response = response;
        let mut body = String::new();
        while !body.contains("\"content\"") {
            let bytes = tokio::time::timeout(Duration::from_secs(2), response.chunk())
                .await
                .expect("stream should deliver text before completion")
                .unwrap()
                .expect("stream ended before text");
            body.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        assert!(
            !body.contains("[DONE]"),
            "text must arrive before completion"
        );
        while let Some(bytes) = response.chunk().await.unwrap() {
            body.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        let mut answer = String::new();
        let mut finish_count = 0;
        for line in body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|line| *line != "[DONE]")
        {
            let chunk: Value = serde_json::from_str(line).unwrap();
            if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
                answer.push_str(text);
            }
            if chunk["choices"][0]["finish_reason"] == "stop" {
                finish_count += 1;
            }
        }
        assert_eq!(answer, buffered["choices"][0]["message"]["content"]);
        assert_eq!(finish_count, 1);
        assert!(body.ends_with("data: [DONE]\n\n"));
        let mut unsupported = input.clone();
        unsupported["temperature"] = json!(0.5);
        assert_eq!(
            client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(&key)
                .json(&unsupported)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let mut unknown = input.clone();
        unknown["model"] = json!("sagaryet/default");
        assert_eq!(
            client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(&key)
                .json(&unknown)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        unknown["model"] = json!("codex/unknown");
        assert_eq!(
            client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(&key)
                .json(&unknown)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .bearer_auth(&key)
                .header("Origin", "https://example.com")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .bearer_auth(&key)
                .header("Host", "attacker.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let rotated = controller.rotate().await.unwrap().access_key.unwrap();
        assert_ne!(key, rotated);
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .bearer_auth(&key)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(format!("{base}/models"))
                .bearer_auth(&rotated)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert!(!controller.stop().await.running);
        assert!(client.get(format!("{base}/models")).send().await.is_err());
        assert!(controller.start(0).await.unwrap().running);
        controller.stop().await;
    }
    #[tokio::test]
    async fn overload_disconnect_and_stop_release_active_work() {
        let controller = Controller::default();
        let status = controller.start(0).await.unwrap();
        let base = status.base_url.unwrap();
        let key = status.access_key.unwrap();
        let client = reqwest::Client::new();
        let input = json!({"model":"synthetic/test","messages":[{"role":"user","content":"hello"}],"stream":true});
        let response = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(controller.status().await.active, 1);
        drop(response);
        for _ in 0..20 {
            if controller.status().await.active == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(controller.status().await.active, 0);
        let response = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&input)
            .send()
            .await
            .unwrap();
        controller.stop().await;
        let body = response.text().await.unwrap();
        assert!(!body.contains("[DONE]"));
    }
    #[tokio::test]
    async fn custom_and_empty_key_setup_is_enforced() {
        let controller = Controller::default();
        let client = reqwest::Client::new();
        let custom = controller
            .start_configured(0, Some("my-own-key".into()), "synthetic/test", None)
            .await
            .unwrap();
        assert_eq!(custom.access_key.as_deref(), Some("my-own-key"));
        let url = format!("{}/models", custom.base_url.unwrap());
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("my-own-key")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("different-key")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        controller.stop().await;
        let open = controller
            .start_configured(0, Some(String::new()), "synthetic/test", None)
            .await
            .unwrap();
        assert_eq!(open.access_key.as_deref(), Some(""));
        let url = format!("{}/models", open.base_url.unwrap());
        for arbitrary in ["sagaryet", "any-other-key"] {
            assert_eq!(
                client
                    .get(&url)
                    .bearer_auth(arbitrary)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            client.get(&url).send().await.unwrap().status(),
            StatusCode::OK
        );
        let rotated = controller.rotate().await.unwrap().access_key.unwrap();
        assert!(!rotated.is_empty());
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("sagaryet")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .get(&url)
                .bearer_auth(rotated)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        controller.stop().await;
    }
    #[tokio::test]
    async fn catalog_lists_all_models_and_requests_choose_the_model() {
        let (_stop, receiver) = watch::channel(false);
        let state = Arc::new(ServerState {
            key: RwLock::new(String::new()),
            slots: Arc::new(Semaphore::new(1)),
            admitted: AtomicU64::new(0),
            stop: receiver,
            model: "synthetic/first".into(),
            models: ["synthetic/first", "synthetic/test"]
                .into_iter()
                .map(|id| crate::catalog::Model {
                    id: id.into(),
                    name: id.into(),
                    provider: "synthetic".into(),
                    reasoning_efforts: vec![],
                    default_effort: None,
                    can_serve: true,
                    streaming: "incremental",
                })
                .collect(),
            reasoning: None,
            admission: Arc::new(Semaphore::new(5)),
            cancel: watch::channel(0).0,
            last_result: std::sync::Mutex::new(None),
            port: 0,
            rate: std::sync::Mutex::new((std::time::Instant::now(), 0)),
        });
        let Json(list) = models(State(state.clone())).await;
        assert_eq!(list["data"].as_array().unwrap().len(), 2);
        assert_eq!(list["data"][0]["id"], "synthetic/first");
        assert_eq!(list["data"][1]["id"], "synthetic/test");
        let response = chat(
            State(state.clone()),
            Ok(Json(Chat {
                model: "synthetic/test".into(),
                messages: vec![Message {
                    role: "user".into(),
                    content: "hello".into(),
                }],
                stream: false,
                n: None,
                reasoning_effort: None,
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 10000)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["model"], "synthetic/test");
        assert!(
            body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("Synthetic test response")
        );
    }
    #[tokio::test]
    async fn invalid_setup_never_starts_server() {
        let controller = Controller::default();
        for invalid in ["invalid key", "newline\nkey", "မင်္ဂလာပါ"] {
            assert!(
                controller
                    .start_configured(0, Some(invalid.into()), "synthetic/test", None)
                    .await
                    .is_err()
            );
        }
        assert!(
            controller
                .start_configured(0, None, "codex/not-enabled", None)
                .await
                .is_err()
        );
        assert!(
            controller
                .start_configured(0, None, "synthetic/test", Some("high".into()))
                .await
                .is_err()
        );
        assert!(!controller.status().await.running);
    }
    #[tokio::test]
    async fn bounded_queue_and_desktop_cancel_release_all_slots() {
        let controller = Controller::default();
        let status = controller.start(0).await.unwrap();
        let base = status.base_url.unwrap();
        let key = status.access_key.unwrap();
        let client = reqwest::Client::new();
        let input = json!({"model":"synthetic/test","messages":[{"role":"user","content":"hello"}],"stream":true});
        let active = client
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&input)
            .send()
            .await
            .unwrap();
        // Access admission directly to test the exact capacity independently of socket scheduling.
        let state = controller.0.lock().await.as_ref().unwrap().state.clone();
        let waiting: Vec<_> = (0..4)
            .map(|_| state.admission.clone().try_acquire_owned().unwrap())
            .collect();
        assert_eq!(controller.status().await.queued, 4);
        assert_eq!(
            client
                .post(format!("{base}/chat/completions"))
                .bearer_auth(&key)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        controller.cancel().await;
        assert!(!active.text().await.unwrap().contains("[DONE]"));
        drop(waiting);
        assert_eq!(controller.status().await.active, 0);
        assert_eq!(controller.status().await.queued, 0);
        controller.stop().await;
    }
    #[tokio::test]
    #[ignore = "installed Codex end-to-end generation; consumes quota"]
    async fn installed_codex_http_history() {
        let controller = Controller::default();
        let status = controller
            .start_network(0, None, "codex/gpt-5.6-sol", None)
            .await
            .unwrap();
        let base = status.base_url.unwrap();
        let key = status.access_key.unwrap();
        let client = reqwest::Client::new();
        let response = client.post(format!("{base}/chat/completions")).bearer_auth(&key).json(&json!({"model":"codex/gpt-5.6-sol","messages":[{"role":"system","content":"Answer in one short sentence."},{"role":"user","content":"My favorite fruit is mango."},{"role":"assistant","content":"Understood."},{"role":"user","content":"What is my favorite fruit?"}]})).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value = response.json().await.unwrap();
        assert!(
            value["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("mango")
        );
        controller.stop().await;
    }
    #[test]
    fn conversation_order() {
        for value in [
            json!({"model":"synthetic/test","messages":[]}),
            json!({"model":"synthetic/test","messages":[{"role":"tool","content":"x"}]}),
            json!({"model":"synthetic/test","messages":[{"role":"assistant","content":"x"}]}),
        ] {
            assert!(validate(&serde_json::from_value::<Chat>(value).unwrap()).is_err());
        }
        let good = json!({"model":"synthetic/test","messages":[{"role":"system","content":"brief"},
            {"role":"user","content":"first"},{"role":"assistant","content":"answer"},{"role":"user","content":"next"}]});
        assert!(validate(&serde_json::from_value::<Chat>(good).unwrap()).is_ok());
    }
}
