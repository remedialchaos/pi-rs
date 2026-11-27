//! Run-scoped pi-messages endpoint. The child receives a loopback token while the
//! selected provider credential and provider configuration stay in the evaluator.
use super::*;
use pi_rs_ai::{
    ModelsOptions,
    auth::{CredentialStore, InMemoryCredentialStore},
};
use pi_rs_coding_agent::core::{
    auth_storage::read_stored_credential, model_registry::ModelRegistry,
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

pub(super) struct Broker {
    pub model: Model,
    pub config: Value,
    pub prompt_hash: Arc<Mutex<Option<String>>>,
    task: JoinHandle<()>,
    cancellation: pi_rs_ai::ProviderRequestOptions,
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.cancellation.signal.cancel();
        self.task.abort();
    }
}

impl Broker {
    pub async fn start(
        selection: &PiCodingAgentModelSelection,
        expected_docs: Option<bool>,
    ) -> EvalResult<Self> {
        let host = pi_rs_coding_agent::config::get_agent_dir().map_err(EvalError::message)?;
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let cancellation = pi_rs_ai::ProviderRequestOptions::default();
        if let Some(credential) =
            read_stored_credential(&selection.provider, &host.join("auth.json"))
        {
            credentials
                .modify(
                    &selection.provider,
                    Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
                    &cancellation.signal,
                )
                .await
                .map_err(|e| EvalError::message(e.to_string()))?;
        }
        let registry = ModelRegistry::with_builtin(
            ModelsOptions {
                credentials,
                ..Default::default()
            },
            Some(host.join("models.json")),
        )
        .map_err(EvalError::message)?;
        if let Some(error) = registry.get_error() {
            return Err(EvalError::message(error));
        }
        let model = registry
            .get(&selection.provider, &selection.id)
            .ok_or_else(|| {
                EvalError::message(format!(
                    "Eval model not found: {}/{}",
                    selection.provider, selection.id
                ))
            })?;
        let mut request = cancellation.clone();
        request.api_key = registry.api_key_for_provider(&selection.provider);
        if request.api_key.is_none()
            && registry
                .models()
                .get_auth(&selection.provider, &Default::default())
                .await
                .map_err(|e| EvalError::message(e.to_string()))?
                .is_none()
        {
            return Err(EvalError::message(format!(
                "Eval model has no configured authentication: {}/{}",
                selection.provider, selection.id
            )));
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let key = uuid::Uuid::new_v4().to_string();
        let config = json!({"api":"pi-messages","apiKey":key,"baseUrl":format!("http://{}",listener.local_addr()?),"models":[{"id":model.id,"name":model.name,"contextWindow":model.context_window,"maxTokens":model.max_tokens}]});
        let models = registry.models().clone();
        let target = model.clone();
        let prompt_hash = Arc::new(Mutex::new(None));
        let observed = prompt_hash.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                if let Err(e) = serve(
                    &mut socket,
                    &key,
                    &models,
                    &target,
                    &request,
                    expected_docs,
                    &observed,
                )
                .await
                {
                    let body = json!({"error":{"message":e.to_string()}}).to_string();
                    let _ = send(&mut socket, "400 Bad Request", "application/json", &body).await;
                }
            }
        });
        Ok(Self {
            model,
            config,
            task,
            cancellation,
            prompt_hash,
        })
    }
}

async fn send(socket: &mut TcpStream, status: &str, media: &str, body: &str) -> EvalResult<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await?;
    Ok(())
}

async fn serve(
    socket: &mut TcpStream,
    key: &str,
    models: &pi_rs_ai::Models,
    model: &Model,
    request: &pi_rs_ai::ProviderRequestOptions,
    expected_docs: Option<bool>,
    observed: &Mutex<Option<String>>,
) -> EvalResult<()> {
    const LIMIT: usize = 32 * 1024 * 1024;
    let mut data = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        if let Some(i) = data.windows(4).position(|s| s == b"\r\n\r\n") {
            break i + 4;
        }
        if data.len() >= 16384 {
            return Err(EvalError::message("Invalid broker request headers"));
        }
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Err(EvalError::message("Incomplete broker request"));
        }
        data.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&data[..end]).map_err(|e| EvalError::message(e.to_string()))?;
    let mut lines = head.lines();
    let authorized = format!("Bearer {key}");
    let mut token = None;
    let mut length = None;
    if lines.next() != Some("POST /messages HTTP/1.1") {
        return Err(EvalError::message("Unknown broker route"));
    }
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Authorization") {
                token = Some(value.trim());
            }
            if name.eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    if token != Some(authorized.as_str()) {
        return Err(EvalError::message("Invalid broker authorization"));
    }
    let length = length
        .filter(|n| *n <= LIMIT)
        .ok_or_else(|| EvalError::message("Invalid broker content length"))?;
    let body = data
        .get(end..)
        .ok_or_else(|| EvalError::message("Invalid broker frame"))?;
    if body.len() > length {
        return Err(EvalError::message("Unexpected broker bytes"));
    }
    let received = data.len();
    data.resize(end + length, 0);
    socket.read_exact(&mut data[received..]).await?;
    #[derive(Deserialize)]
    struct Request {
        model: String,
        context: Context,
        #[serde(default)]
        options: Value,
    }
    let mut input: Request = serde_json::from_slice(&data[end..])?;
    if input.model != model.id {
        return Err(EvalError::message("Unselected broker model"));
    }
    if expected_docs == Some(false) {
        for message in &mut input.context.messages {
            if let Message::System(s) = message
                && let Some(sections) = &mut s.sections
            {
                sections.shift_remove("docs");
            }
        }
    }
    let prompt = pi_rs_ai::transcript::get_current_system_prompt(&input.context.messages);
    verify_system_prompt(&prompt, None, expected_docs)?;
    *observed.lock().unwrap_or_else(|e| e.into_inner()) = Some(sha256_hex(prompt.as_bytes()));
    let options = StreamOptions {
        request: request.clone(),
        session_id: input.options["sessionId"].as_str().map(str::to_owned),
        max_tokens: input.options["maxTokens"]
            .as_u64()
            .map(|n| n.min(model.max_tokens)),
        ..Default::default()
    };
    let msg = models
        .complete_simple(model.clone(), input.context, options)
        .await
        .map_err(|e| EvalError::message(e.to_string()))?;
    let mut events = vec![json!({"type":"start"})];
    for (i, part) in msg.content.iter().enumerate() {
        match part {
            AssistantContent::Text(t) => {
                events.push(json!({"type":"text_start","contentIndex":i}));
                events.push(json!({"type":"text_delta","contentIndex":i,"delta":t.text}));
                events.push(json!({"type":"text_end","contentIndex":i,"content":t.text,"contentSignature":t.text_signature}));
            }
            AssistantContent::Thinking(t) => {
                events.push(json!({"type":"thinking_start","contentIndex":i}));
                events.push(json!({"type":"thinking_delta","contentIndex":i,"delta":t.thinking}));
                events.push(json!({"type":"thinking_end","contentIndex":i,"content":t.thinking,"contentSignature":t.thinking_signature,"redacted":t.redacted}));
            }
            AssistantContent::ToolCall(c) => {
                events.push(
                    json!({"type":"toolcall_start","contentIndex":i,"id":c.id,"toolName":c.name}),
                );
                events.push(json!({"type":"toolcall_end","contentIndex":i,"toolCall":c}));
            }
        }
    }
    events.push(json!({"type":if msg.stop_reason.failed() {"error"} else {"done"},"reason":msg.stop_reason,"usage":msg.usage,"responseId":msg.response_id,"responseModel":msg.response_model,"errorMessage":msg.error_message}));
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }
    send(socket, "200 OK", "text/event-stream", &body).await
}
