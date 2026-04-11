pub mod scenarios;
pub mod transformer;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};

/// Per-token state: a list of responses and a cursor tracking the next response to return.
struct TokenState {
    responses: Vec<Value>,
    cursor: usize,
}

type SharedState = Arc<Mutex<HashMap<String, TokenState>>>;

pub struct MockServer {
    addr: SocketAddr,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl MockServer {
    /// Start the mock server with pre-configured per-token responses.
    ///
    /// `entries` is a list of (bearer_token, responses) pairs.
    /// Each GET /backend-api/wham/usage request with a matching Bearer token
    /// returns the next response in the list, advancing the cursor.
    /// If the cursor exceeds the list length, the last response is repeated.
    pub async fn start(entries: Vec<(String, Vec<Value>)>) -> Self {
        let mut state_map = HashMap::new();
        for (token, responses) in entries {
            state_map.insert(token, TokenState { responses, cursor: 0 });
        }
        let state: SharedState = Arc::new(Mutex::new(state_map));

        let app = Router::new()
            .route("/backend-api/wham/usage", get(usage_handler))
            .route("/oauth/token", post(token_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        Self { addr, shutdown_tx }
    }

    /// The base URL for setting CS_USAGE_URL (e.g. "http://127.0.0.1:PORT").
    pub fn usage_url(&self) -> String {
        format!("http://{}/backend-api/wham/usage", self.addr)
    }

    /// The base URL for setting CS_TOKEN_URL.
    pub fn token_url(&self) -> String {
        format!("http://{}/oauth/token", self.addr)
    }

    /// Shut down the mock server.
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// Extract Bearer token from Authorization header.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}

/// GET /backend-api/wham/usage handler.
async fn usage_handler(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "Missing Bearer token").into_response(),
    };

    let mut map = state.lock().unwrap();
    let ts = match map.get_mut(&token) {
        Some(ts) => ts,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                format!("Unknown token: {token}"),
            )
                .into_response()
        }
    };

    let idx = ts.cursor.min(ts.responses.len().saturating_sub(1));
    let response = ts.responses[idx].clone();
    ts.cursor += 1;

    axum::Json(response).into_response()
}

/// POST /oauth/token handler — mock token refresh.
/// Always succeeds, returning dummy tokens based on the refresh_token in the form body.
async fn token_handler(body: String) -> impl IntoResponse {
    // Parse the form body to extract refresh_token for traceability
    let refresh_token = body
        .split('&')
        .find_map(|pair| {
            let (key, val) = pair.split_once('=')?;
            if key == "refresh_token" {
                Some(urlencoding::decode(val).unwrap_or_default().into_owned())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    let response = json!({
        "id_token": format!("mock_id_{refresh_token}"),
        "access_token": format!("mock_access_{refresh_token}"),
        "refresh_token": format!("mock_refresh_{refresh_token}"),
    });

    axum::Json(response)
}
