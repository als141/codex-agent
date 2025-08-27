use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use axum::routing::get;
use axum::routing::post;
use codex_core::ModelClient;
use codex_core::ModelProviderInfo;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::ConfigToml;
use codex_core::user_agent::get_codex_user_agent;
use codex_login::AuthManager;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tower_http::cors::Any;
use tower_http::cors::CorsLayer;
use tracing::Level;
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    auth: Arc<AuthManager>,
    provider: ModelProviderInfo,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    messages: Vec<ChatMessage>,
    model: Option<String>,
    stream: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    message: ChatMessage,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_max_level(Level::INFO)
        .init();

    let cfg = ConfigToml::default();
    let overrides = ConfigOverrides::default();
    let config = Config::load_from_base_config_with_overrides(
        cfg,
        overrides,
        codex_core::config::find_codex_home()?,
    )?;
    let config = Arc::new(config);

    let auth = AuthManager::shared(config.codex_home.clone(), config.preferred_auth_method);

    let provider = config.model_provider.clone();

    let state = AppState {
        config: config.clone(),
        auth,
        provider,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/chat", post(chat))
        .route("/chat/stream", post(chat_stream))
        .layer(cors)
        .with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, ua = get_codex_user_agent(None), "codex api server starting");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (axum::http::StatusCode::OK, "ok")
}

async fn chat(
    State(app): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (axum::http::StatusCode, String)> {
    let mut prompt = Prompt::default();
    for m in req.messages {
        prompt
            .input
            .push(codex_protocol::models::ResponseItem::Message {
                id: None,
                role: m.role,
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: m.content,
                }],
            });
    }

    let session_id = Uuid::new_v4();
    let client = Arc::new(ModelClient::new(
        app.config.clone(),
        Some(app.auth.clone()),
        app.provider.clone(),
        app.config.model_reasoning_effort,
        app.config.model_reasoning_summary,
        session_id,
    ));

    let stream = client
        .clone()
        .stream_owned(prompt)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e.to_string()))?;

    let text = collect_final_message(stream).await.unwrap_or_default();
    Ok(Json(ChatResponse {
        message: ChatMessage {
            role: "assistant".to_string(),
            content: text,
        },
    }))
}

async fn chat_stream(
    State(app): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (axum::http::StatusCode, String),
> {
    let mut prompt = Prompt::default();
    for m in req.messages {
        prompt
            .input
            .push(codex_protocol::models::ResponseItem::Message {
                id: None,
                role: m.role,
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: m.content,
                }],
            });
    }

    let session_id = Uuid::new_v4();
    let client = Arc::new(ModelClient::new(
        app.config.clone(),
        Some(app.auth.clone()),
        app.provider.clone(),
        app.config.model_reasoning_effort,
        app.config.model_reasoning_summary,
        session_id,
    ));
    let response_stream = client
        .clone()
        .stream_owned(prompt)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, e.to_string()))?;

    let sse_stream = response_stream.map(|ev_res| {
        let event = match ev_res {
            Ok(ResponseEvent::OutputTextDelta(delta)) => Event::default().event("delta").data(delta),
            Ok(ResponseEvent::ReasoningSummaryDelta(delta)) => {
                Event::default().event("reasoning_summary").data(delta)
            }
            Ok(ResponseEvent::ReasoningContentDelta(delta)) => {
                Event::default().event("reasoning").data(delta)
            }
            Ok(ResponseEvent::OutputItemDone(item)) => Event::default()
                .event("item")
                .json_data(&item)
                .unwrap_or(Event::default().data("")),
            Ok(ResponseEvent::Created) => Event::default().event("created").data(""),
            Ok(ResponseEvent::Completed { .. }) => {
                Event::default().event("completed").data("")
            }
            Ok(ResponseEvent::ReasoningSummaryPartAdded) => {
                Event::default().event("reasoning_summary_part").data("")
            }
            Ok(ResponseEvent::WebSearchCallBegin { call_id, .. }) => {
                Event::default().event("web_search_call_begin").data(call_id)
            }
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        Ok::<Event, std::convert::Infallible>(event)
    });

    let sse = Sse::new(sse_stream).keep_alive(axum::response::sse::KeepAlive::default());
    Ok(sse)
}

async fn collect_final_message(
    mut stream: impl futures::Stream<Item = codex_core::error::Result<ResponseEvent>> + Unpin,
) -> Option<String> {
    use futures::StreamExt;
    let mut last = None;
    while let Some(ev) = stream.next().await.transpose().ok().flatten() {
        match ev {
            ResponseEvent::OutputItemDone(item) => {
                if let codex_protocol::models::ResponseItem::Message { content, .. } = item {
                    let mut buf = String::new();
                    for c in content {
                        if let codex_protocol::models::ContentItem::OutputText { text } = c {
                            buf.push_str(&text);
                        }
                    }
                    last = Some(buf);
                }
            }
            ResponseEvent::Completed { .. } => {}
            _ => {}
        }
    }
    last
}

// stream_to_sse no longer needed; mapping is done inline above.
