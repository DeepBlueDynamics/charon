//! charon-gateway core library (spec 09/12).

use axum::{
    extract::FromRequestParts,
    extract::ws::{WebSocketUpgrade, WebSocket, Message},
    http::{request::Parts, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use uuid::Uuid;
use charon_core::{Frame, Envelope, ModelCard, Keybind, ErrorCode};
use charon_core::wire::Payout;
use charon_core::payment::Rate;

// Authenticator seam (object-safe)
pub trait Authenticator: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ErrorCode>> + Send + 'a>>;
}

// PaymentVerifier seam
pub trait PaymentVerifier: Send + Sync {
    fn verify_payment(
        &self,
        payment: &charon_core::wire::Payment,
        expected_total_msat: u64,
    ) -> Result<(), ErrorCode>;
}

pub struct DevPaymentVerifier;

impl PaymentVerifier for DevPaymentVerifier {
    fn verify_payment(
        &self,
        _payment: &charon_core::wire::Payment,
        _expected_total_msat: u64,
    ) -> Result<(), ErrorCode> {
        Ok(())
    }
}

pub struct GnosisAuthenticator {
    nuts: charon_core::auth::NutsAuth,
}

impl GnosisAuthenticator {
    pub fn new(auth_url: String, disable_auth: bool) -> Self {
        Self {
            nuts: charon_core::auth::NutsAuth::new(auth_url, disable_auth),
        }
    }
}

impl Authenticator for GnosisAuthenticator {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ErrorCode>> + Send + 'a>> {
        Box::pin(async move {
            if self.nuts.disable_auth {
                let principal = if let Some(stripped) = token.strip_prefix("ahp_") {
                    stripped.to_string()
                } else {
                    token.to_string()
                };
                Ok(principal)
            } else {
                self.nuts.validate(token).await
                    .map_err(|_| ErrorCode::AuthFailed)
            }
        })
    }
}

#[derive(Clone)]
pub struct ProviderConnection {
    pub principal: String,
    pub models: Vec<ModelCard>,
    #[allow(dead_code)]
    pub keybind: Keybind,
    #[allow(dead_code)]
    pub payout: Payout,
    pub connection_id: Uuid,
}

#[derive(Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub consumer_principal: String,
    pub consumer_connection_id: Uuid,
    pub provider_principal: String,
    pub provider_connection_id: Uuid,
    #[allow(dead_code)]
    pub envelope: Envelope,
    pub total_msat: u64,
    pub provider_msat: u64,
    pub gateway_msat: u64,
}

#[derive(Clone)]
pub struct ConnectionInfo {
    #[allow(dead_code)]
    pub id: Uuid,
    pub principal: Option<String>,
    pub sender: tokio::sync::mpsc::UnboundedSender<Frame>,
    #[allow(dead_code)]
    pub ip: IpAddr,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalletEntry {
    pub ts: u64,
    #[serde(rename = "type")]
    pub r#type: String,
    pub amount_msat: i64,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct UserWallet {
    pub balance_msat: u64,
    pub history: Vec<WalletEntry>,
}

pub struct GatewayState {
    pub providers: Mutex<HashMap<String, ProviderConnection>>,
    pub sessions: Mutex<HashMap<String, SessionInfo>>,
    pub principal_sessions: Mutex<HashMap<String, HashSet<String>>>,
    pub connections: Mutex<HashMap<Uuid, ConnectionInfo>>,
    pub rate_limits: Mutex<HashMap<String, Vec<tokio::time::Instant>>>,
    pub wallets: Mutex<HashMap<String, UserWallet>>,
    pub authenticator: Arc<dyn Authenticator>,
    pub payment_verifier: Arc<dyn PaymentVerifier>,
    pub disable_auth: bool,
    pub markup_bps: u64,
    pub floor_msat: u64,
}

impl GatewayState {
    pub fn new(
        authenticator: Arc<dyn Authenticator>,
        payment_verifier: Arc<dyn PaymentVerifier>,
        disable_auth: bool,
        markup_bps: u64,
        floor_msat: u64,
    ) -> Self {
        Self {
            providers: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            principal_sessions: Mutex::new(HashMap::new()),
            connections: Mutex::new(HashMap::new()),
            rate_limits: Mutex::new(HashMap::new()),
            wallets: Mutex::new(HashMap::new()),
            authenticator,
            payment_verifier,
            disable_auth,
            markup_bps,
            floor_msat,
        }
    }

    pub fn record_wallet_event(&self, principal: &str, kind: &str, amount_msat: i64, status: &str) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = WalletEntry {
            ts,
            r#type: kind.to_string(),
            amount_msat,
            status: status.to_string(),
        };

        let mut wallets = self.wallets.lock().unwrap();
        let wallet = wallets.entry(principal.to_string()).or_insert_with(|| UserWallet {
            balance_msat: 10_000_000_000,
            history: Vec::new(),
        });

        if amount_msat >= 0 {
            wallet.balance_msat = wallet.balance_msat.saturating_add(amount_msat as u64);
        } else {
            let amount_abs = amount_msat.unsigned_abs();
            wallet.balance_msat = wallet.balance_msat.saturating_sub(amount_abs);
        }

        wallet.history.push(entry);
    }

    pub fn check_ip_connect_rate_limit(&self, ip: IpAddr) -> bool {
        self.check_rate_limit(&format!("ip_connect:{}", ip), 30)
    }

    pub fn check_principal_register_rate_limit(&self, principal: &str) -> bool {
        self.check_rate_limit(&format!("register:{}", principal), 30)
    }

    pub fn check_consumer_open_rate_limit(&self, principal: &str) -> bool {
        self.check_rate_limit(&format!("open:{}", principal), 60)
    }

    pub fn check_rate_limit(&self, key: &str, limit: usize) -> bool {
        let now = tokio::time::Instant::now();
        let mut rate_limits = self.rate_limits.lock().unwrap();
        let entries = rate_limits.entry(key.to_string()).or_default();
        entries.retain(|&t| now.duration_since(t) < tokio::time::Duration::from_secs(60));
        if entries.len() >= limit {
            false
        } else {
            entries.push(now);
            true
        }
    }

    pub fn get_active_session_count(&self, principal: &str) -> usize {
        let principal_sessions = self.principal_sessions.lock().unwrap();
        principal_sessions.get(principal).map(|s| s.len()).unwrap_or(0)
    }

    pub fn add_connection(&self, id: Uuid, principal: Option<String>, sender: tokio::sync::mpsc::UnboundedSender<Frame>, ip: IpAddr) {
        let mut connections = self.connections.lock().unwrap();
        connections.insert(id, ConnectionInfo { id, principal, sender, ip });
    }

    pub fn get_connection(&self, id: Uuid) -> Option<ConnectionInfo> {
        let connections = self.connections.lock().unwrap();
        connections.get(&id).cloned()
    }

    pub fn update_connection_principal(&self, id: Uuid, principal: Option<String>) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(conn) = connections.get_mut(&id) {
            conn.principal = principal;
        }
    }

    pub fn send_to_connection(&self, id: Uuid, frame: Frame) -> bool {
        let connections = self.connections.lock().unwrap();
        if let Some(conn) = connections.get(&id) {
            conn.sender.send(frame).is_ok()
        } else {
            false
        }
    }

    pub fn remove_connection(&self, id: Uuid) {
        let principal_opt = {
            let mut connections = self.connections.lock().unwrap();
            connections.remove(&id).and_then(|c| c.principal)
        };

        if let Some(ref principal) = principal_opt {
            let mut providers = self.providers.lock().unwrap();
            if let Some(p) = providers.get(principal) {
                if p.connection_id == id {
                    providers.remove(principal);
                }
            }
        }

        let mut sessions_to_cancel = Vec::new();
        {
            let sessions = self.sessions.lock().unwrap();
            for s in sessions.values() {
                if s.consumer_connection_id == id || s.provider_connection_id == id {
                    sessions_to_cancel.push(s.session_id.clone());
                }
            }
        }

        for session_id in sessions_to_cancel {
            self.cancel_session(&session_id);
        }
    }

    pub fn register_provider(&self, principal: String, models: Vec<ModelCard>, keybind: Keybind, payout: Payout, connection_id: Uuid) {
        let old_conn_id = {
            let mut providers = self.providers.lock().unwrap();
            providers.insert(principal.clone(), ProviderConnection {
                principal,
                models,
                keybind,
                payout,
                connection_id,
            }).map(|p| p.connection_id)
        };

        if let Some(old_id) = old_conn_id {
            if old_id != connection_id {
                self.remove_connection(old_id);
            }
        }
    }

    pub fn get_provider(&self, principal: &str) -> Option<ProviderConnection> {
        let providers = self.providers.lock().unwrap();
        providers.get(principal).cloned()
    }

    pub fn add_session(&self, session: SessionInfo) {
        let session_id = session.session_id.clone();
        let consumer = session.consumer_principal.clone();
        let provider = session.provider_principal.clone();
        
        self.sessions.lock().unwrap().insert(session_id.clone(), session);
        
        let mut principal_sessions = self.principal_sessions.lock().unwrap();
        principal_sessions.entry(consumer).or_default().insert(session_id.clone());
        principal_sessions.entry(provider).or_default().insert(session_id.clone());
    }

    pub fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_id).cloned()
    }

    pub fn remove_session(&self, session_id: &str) {
        if let Some(session) = self.sessions.lock().unwrap().remove(session_id) {
            let mut principal_sessions = self.principal_sessions.lock().unwrap();
            
            if let Some(set) = principal_sessions.get_mut(&session.consumer_principal) {
                set.remove(session_id);
            }
            if let Some(set) = principal_sessions.get_mut(&session.provider_principal) {
                set.remove(session_id);
            }
        }
    }

    pub fn cancel_session(&self, session_id: &str) {
        if let Some(session) = self.sessions.lock().unwrap().remove(session_id) {
            let mut principal_sessions = self.principal_sessions.lock().unwrap();
            
            if let Some(set) = principal_sessions.get_mut(&session.consumer_principal) {
                set.remove(session_id);
            }
            if let Some(set) = principal_sessions.get_mut(&session.provider_principal) {
                set.remove(session_id);
            }

            let cancel_frame = Frame::Cancel { session_id: session_id.to_string() };
            let connections = self.connections.lock().unwrap();
            if let Some(conn) = connections.get(&session.consumer_connection_id) {
                let _ = conn.sender.send(cancel_frame.clone());
            }
            if let Some(conn) = connections.get(&session.provider_connection_id) {
                let _ = conn.sender.send(cancel_frame);
            }
        }
    }
}

pub struct HttpPrincipal(pub String);

impl FromRequestParts<Arc<GatewayState>> for HttpPrincipal {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<GatewayState>,
    ) -> Result<Self, Self::Rejection> {
        if state.disable_auth {
            return Ok(HttpPrincipal(charon_core::auth::NutsAuth::dev_principal().to_string()));
        }
        
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header".to_string()))?;
            
        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or((StatusCode::UNAUTHORIZED, "Invalid Authorization header format".to_string()))?
            .trim();
            
        match state.authenticator.authenticate(token).await {
            Ok(principal) => Ok(HttpPrincipal(principal)),
            Err(e) => Err((StatusCode::UNAUTHORIZED, format!("Authentication failed: {:?}", e))),
        }
    }
}

#[derive(serde::Serialize)]
pub struct DirectoryEntry {
    pub principal: String,
    pub models: Vec<ModelCard>,
}

pub async fn get_directory(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    _principal: HttpPrincipal,
) -> axum::Json<Vec<DirectoryEntry>> {
    let providers = state.providers.lock().unwrap();
    let entries: Vec<DirectoryEntry> = providers
        .values()
        .map(|p| DirectoryEntry {
            principal: p.principal.clone(),
            models: p.models.clone(),
        })
        .collect();
    axum::Json(entries)
}

#[derive(serde::Serialize)]
pub struct ReputationResponse {
    pub ratings: Vec<serde_json::Value>,
    pub average_score: f64,
    pub total_settled_msat: u64,
}

pub async fn get_reputation(
    axum::extract::State(_state): axum::extract::State<Arc<GatewayState>>,
    axum::extract::Path(principal): axum::extract::Path<String>,
    _principal: HttpPrincipal,
) -> axum::Json<ReputationResponse> {
    tracing::info!(%principal, "Fetching reputation for provider");
    axum::Json(ReputationResponse {
        ratings: vec![],
        average_score: 0.0,
        total_settled_msat: 0,
    })
}

#[derive(serde::Deserialize)]
pub struct QuoteRequest {
    pub model: String,
    pub est_input_tokens: u32,
    pub max_tokens: u32,
}

#[derive(serde::Serialize)]
pub struct QuoteResponse {
    pub provider_msat: u64,
    pub gateway_msat: u64,
    pub total_msat: u64,
}

pub async fn post_quote(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    _principal: HttpPrincipal,
    axum::Json(req): axum::Json<QuoteRequest>,
) -> Result<axum::Json<QuoteResponse>, (StatusCode, String)> {
    let providers = state.providers.lock().unwrap();
    let mut rate_opt = None;
    for p in providers.values() {
        if let Some(m) = p.models.iter().find(|m| m.name == req.model) {
            rate_opt = Some(Rate {
                price_msat_per_mtok_in: m.price_msat_per_mtok_in,
                price_msat_per_mtok_out: m.price_msat_per_mtok_out,
            });
            break;
        }
    }

    let rate = rate_opt.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Model {} not offered by any connected provider", req.model),
        )
    })?;

    let quote = charon_core::payment::quote(
        rate,
        req.est_input_tokens,
        req.max_tokens,
        state.markup_bps,
        state.floor_msat,
    );

    Ok(axum::Json(QuoteResponse {
        provider_msat: quote.provider_msat,
        gateway_msat: quote.gateway_msat,
        total_msat: quote.total_msat,
    }))
}

pub async fn wallet_deposit(
    _principal: HttpPrincipal,
) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "TODO: POST /v1/wallet/deposit (spec 12)")
}

#[derive(serde::Serialize)]
pub struct BalanceResponse {
    pub balance_msat: u64,
}

pub async fn wallet_balance(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    principal: HttpPrincipal,
) -> axum::Json<BalanceResponse> {
    let mut wallets = state.wallets.lock().unwrap();
    let wallet = wallets.entry(principal.0.clone()).or_insert_with(|| UserWallet {
        balance_msat: 10_000_000_000,
        history: Vec::new(),
    });
    axum::Json(BalanceResponse {
        balance_msat: wallet.balance_msat,
    })
}

#[derive(serde::Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<WalletEntry>,
}

pub async fn wallet_history(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    principal: HttpPrincipal,
) -> axum::Json<HistoryResponse> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now.saturating_sub(14 * 24 * 60 * 60);

    let mut wallets = state.wallets.lock().unwrap();
    let wallet = wallets.entry(principal.0.clone()).or_insert_with(|| UserWallet {
        balance_msat: 10_000_000_000,
        history: Vec::new(),
    });

    wallet.history.retain(|e| e.ts >= cutoff);

    axum::Json(HistoryResponse {
        entries: wallet.history.clone(),
    })
}

pub async fn post_ratings(
    _principal: HttpPrincipal,
) -> (StatusCode, &'static str) {
    (StatusCode::NOT_IMPLEMENTED, "TODO: POST /v1/ratings (spec 12)")
}

pub async fn ws_handler(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let mut extracted_token = None;
    if let Some(auth_val) = headers.get("Authorization") {
        if let Ok(auth_str) = auth_val.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                extracted_token = Some(token.trim().to_string());
            } else {
                extracted_token = Some(auth_str.trim().to_string());
            }
        }
    }
    
    if extracted_token.is_none() {
        if let Some(query) = uri.query() {
            for pair in query.split('&') {
                let mut parts = pair.splitn(2, '=');
                if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                    if key == "token" || key == "ahp_token" {
                        extracted_token = Some(val.to_string());
                        break;
                    }
                }
            }
        }
    }

    let mut initial_principal = None;
    if let Some(token) = extracted_token {
        match state.authenticator.authenticate(&token).await {
            Ok(principal) => {
                initial_principal = Some(principal);
            }
            Err(err_code) => {
                let err_frame = Frame::Error {
                    session_id: None,
                    code: err_code,
                    message: "Authentication failed".into(),
                    http_status: Some(401),
                };
                let body = serde_json::to_string(&err_frame).unwrap_or_default();
                return (axum::http::StatusCode::UNAUTHORIZED, body).into_response();
            }
        }
    }

    ws.on_upgrade(move |socket| handle_ws_socket(state, socket, addr, initial_principal))
}

async fn handle_ws_socket(
    state: Arc<GatewayState>,
    socket: WebSocket,
    addr: std::net::SocketAddr,
    initial_principal: Option<String>,
) {
    let ip = addr.ip();
    
    if !state.check_ip_connect_rate_limit(ip) {
        tracing::warn!(%ip, "Connection rate limited");
        return;
    }

    let connection_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    
    state.add_connection(connection_id, initial_principal.clone(), tx.clone(), ip);
    tracing::info!(%connection_id, principal = ?initial_principal, "Connection established");

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let last_pong_received = Arc::new(std::sync::atomic::AtomicBool::new(true));
    
    let mut rx_task = rx;
    let ws_send_loop = async move {
        while let Some(frame) = rx_task.recv().await {
            if let Ok(json_str) = serde_json::to_string(&frame) {
                if let Err(e) = ws_sender.send(Message::Text(json_str.into())).await {
                    tracing::error!(error = ?e, "Failed to send WebSocket message");
                    break;
                }
            }
        }
        let _ = ws_sender.close().await;
    };
    
    let reader_state = state.clone();
    let reader_conn_id = connection_id;
    let reader_last_pong = last_pong_received.clone();
    let tx_clone = tx.clone();
    
    let mut ping_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    ping_interval.tick().await;

    let ws_read_loop = async move {
        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    if !reader_last_pong.load(std::sync::atomic::Ordering::Relaxed) {
                        tracing::warn!(%reader_conn_id, "Missed Pong, closing connection");
                        break;
                    }
                    reader_last_pong.store(false, std::sync::atomic::Ordering::Relaxed);
                    if let Err(_) = tx_clone.send(Frame::Ping) {
                        break;
                    }
                }
                msg_opt = ws_receiver.next() => {
                    match msg_opt {
                        Some(Ok(msg)) => {
                            match msg {
                                Message::Text(text) => {
                                    match serde_json::from_str::<Frame>(&text) {
                                        Ok(frame) => {
                                            if let Err(e) = process_frame(&reader_state, reader_conn_id, frame, &tx_clone, &reader_last_pong).await {
                                                tracing::warn!(error = ?e, "Failed to process frame");
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = ?e, "Failed to parse frame");
                                            let err_frame = Frame::Error {
                                                session_id: None,
                                                code: ErrorCode::EnvelopeMismatch,
                                                message: format!("Parse error: {}", e),
                                                http_status: Some(400),
                                            };
                                            let _ = tx_clone.send(err_frame);
                                        }
                                    }
                                }
                                Message::Close(_) => {
                                    break;
                                }
                                _ => {}
                            }
                        }
                        Some(Err(e)) => {
                            tracing::error!(error = ?e, "WebSocket read error");
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }
    };
    
    tokio::select! {
        _ = ws_send_loop => {},
        _ = ws_read_loop => {},
    }
    
    tracing::info!(%connection_id, "Cleaning up connection");
    state.remove_connection(connection_id);
}

async fn process_frame(
    state: &Arc<GatewayState>,
    connection_id: Uuid,
    frame: Frame,
    tx: &tokio::sync::mpsc::UnboundedSender<Frame>,
    last_pong_received: &Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    match frame {
        Frame::Pong => {
            last_pong_received.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        Frame::Register { ahp_token, keybind, models, payout } => {
            let _conn_info = state.get_connection(connection_id).ok_or_else(|| anyhow::anyhow!("Connection not found"))?;
            
            match state.authenticator.authenticate(&ahp_token).await {
                Ok(principal) => {
                    if !state.check_principal_register_rate_limit(&principal) {
                        let err_frame = Frame::Error {
                            session_id: None,
                            code: ErrorCode::RateLimited,
                            message: "Register rate limit exceeded (30/min)".into(),
                            http_status: Some(429),
                        };
                        let _ = tx.send(err_frame);
                        return Ok(());
                    }
                    
                    state.register_provider(
                        principal.clone(),
                        models,
                        keybind,
                        payout,
                        connection_id,
                    );
                    
                    state.update_connection_principal(connection_id, Some(principal.clone()));
                    let _ = tx.send(Frame::Registered { provider: principal });
                }
                Err(err_code) => {
                    let err_frame = Frame::Error {
                        session_id: None,
                        code: err_code,
                        message: "Registration authentication failed".into(),
                        http_status: Some(401),
                    };
                    let _ = tx.send(err_frame);
                }
            }
            Ok(())
        }
        Frame::Open { session_id, mut envelope } => {
            let conn_info = state.get_connection(connection_id).ok_or_else(|| anyhow::anyhow!("Connection not found"))?;
            
            let consumer_principal = match conn_info.principal {
                Some(ref p) => p.clone(),
                None => {
                    if state.disable_auth {
                        charon_core::auth::NutsAuth::dev_principal().to_string()
                    } else {
                        let err_frame = Frame::Error {
                            session_id: Some(session_id.clone()),
                            code: ErrorCode::AuthFailed,
                            message: "Consumer not authenticated".into(),
                            http_status: Some(401),
                        };
                        let _ = tx.send(err_frame);
                        return Ok(());
                    }
                }
            };

            if state.disable_auth {
                envelope.consumer = consumer_principal.clone();
            } else if envelope.consumer != consumer_principal {
                let err_frame = Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: ErrorCode::AuthFailed,
                    message: "Envelope consumer principal mismatch".into(),
                    http_status: Some(401),
                };
                let _ = tx.send(err_frame);
                return Ok(());
            }
            
            if !state.check_consumer_open_rate_limit(&consumer_principal) {
                let err_frame = Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: ErrorCode::RateLimited,
                    message: "Open session rate limit exceeded (60/min)".into(),
                    http_status: Some(429),
                };
                let _ = tx.send(err_frame);
                return Ok(());
            }
            
            if state.get_active_session_count(&consumer_principal) >= 5 {
                let err_frame = Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: ErrorCode::RateLimited,
                    message: "Consumer concurrent session limit exceeded (5)".into(),
                    http_status: Some(429),
                };
                let _ = tx.send(err_frame);
                return Ok(());
            }
            
            let provider = match state.get_provider(&envelope.provider) {
                Some(p) => p,
                None => {
                    let err_frame = Frame::Error {
                        session_id: Some(session_id.clone()),
                        code: ErrorCode::NoProvider,
                        message: format!("Provider {} not found in directory", envelope.provider),
                        http_status: Some(404),
                    };
                    let _ = tx.send(err_frame);
                    return Ok(());
                }
            };
            
            let model_card = match provider.models.iter().find(|m| m.name == envelope.model) {
                Some(mc) => mc,
                None => {
                    let err_frame = Frame::Error {
                        session_id: Some(session_id.clone()),
                        code: ErrorCode::UnknownModel,
                        message: format!("Model {} not supported by provider", envelope.model),
                        http_status: Some(404),
                    };
                    let _ = tx.send(err_frame);
                    return Ok(());
                }
            };
            
            if state.get_active_session_count(&provider.principal) >= 5 {
                let err_frame = Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: ErrorCode::RateLimited,
                    message: "Provider concurrent session limit exceeded (5)".into(),
                    http_status: Some(429),
                };
                let _ = tx.send(err_frame);
                return Ok(());
            }
            
            let rate = Rate {
                price_msat_per_mtok_in: model_card.price_msat_per_mtok_in,
                price_msat_per_mtok_out: model_card.price_msat_per_mtok_out,
            };
            let quote = charon_core::payment::quote(
                rate,
                envelope.est_input_tokens,
                envelope.max_tokens,
                state.markup_bps,
                state.floor_msat,
            );
            
            if let Err(err_code) = state.payment_verifier.verify_payment(&envelope.payment, quote.total_msat) {
                let err_frame = Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: err_code,
                    message: "Payment verification failed".into(),
                    http_status: Some(402),
                };
                let _ = tx.send(err_frame);
                return Ok(());
            }
            
            let session_info = SessionInfo {
                session_id: session_id.clone(),
                consumer_principal: consumer_principal.clone(),
                consumer_connection_id: connection_id,
                provider_principal: provider.principal.clone(),
                provider_connection_id: provider.connection_id,
                envelope: envelope.clone(),
                total_msat: quote.total_msat,
                provider_msat: quote.provider_msat,
                gateway_msat: quote.gateway_msat,
            };
            
            state.add_session(session_info);
            
            let _ = tx.send(Frame::OpenOk {
                session_id: session_id.clone(),
                total_msat: quote.total_msat,
            });
            
            let deliver_frame = Frame::Deliver {
                session_id: session_id.clone(),
                frame: Box::new(Frame::Open {
                    session_id: session_id.clone(),
                    envelope,
                }),
            };
            if let Some(prov_conn) = state.get_connection(provider.connection_id) {
                let _ = prov_conn.sender.send(deliver_frame);
            }
            
            Ok(())
        }
        Frame::Hs { session_id, blob } => {
            relay_opaque(state, connection_id, session_id, Frame::Hs { session_id: "".into(), blob }).await;
            Ok(())
        }
        Frame::Req { session_id, blob } => {
            relay_opaque(state, connection_id, session_id, Frame::Req { session_id: "".into(), blob }).await;
            Ok(())
        }
        Frame::ResHead { session_id, blob } => {
            relay_opaque(state, connection_id, session_id, Frame::ResHead { session_id: "".into(), blob }).await;
            Ok(())
        }
        Frame::Res { session_id, blob } => {
            relay_opaque(state, connection_id, session_id, Frame::Res { session_id: "".into(), blob }).await;
            Ok(())
        }
        Frame::ResEnd { session_id, usage } => {
            let session = match state.get_session(&session_id) {
                Some(s) => s,
                None => {
                    let _ = tx.send(Frame::Error {
                        session_id: Some(session_id.clone()),
                        code: ErrorCode::AuthFailed,
                        message: "Session not found".into(),
                        http_status: Some(404),
                    });
                    return Ok(());
                }
            };
            
            let conn_info = match state.get_connection(connection_id) {
                Some(c) => c,
                None => return Ok(()),
            };
            let sender_principal = conn_info.principal.as_deref().unwrap_or(charon_core::auth::NutsAuth::dev_principal());
            if sender_principal != session.provider_principal {
                let _ = tx.send(Frame::Error {
                    session_id: Some(session_id.clone()),
                    code: ErrorCode::AuthFailed,
                    message: "Sender is not authorized provider for this session".into(),
                    http_status: Some(403),
                });
                return Ok(());
            }
            
            let consumer_conn = state.get_connection(session.consumer_connection_id);
            if let Some(ref cc) = consumer_conn {
                let deliver_frame = Frame::Deliver {
                    session_id: session_id.clone(),
                    frame: Box::new(Frame::ResEnd {
                        session_id: session_id.clone(),
                        usage: usage.clone(),
                    }),
                };
                let _ = cc.sender.send(deliver_frame);
            }
            
            let settled_frame = Frame::Settled {
                session_id: session_id.clone(),
                total_msat: session.total_msat,
                gateway_msat: session.gateway_msat,
                provider_msat: session.provider_msat,
                outcome: "ok".into(),
            };
            
            let _ = tx.send(settled_frame.clone());
            if let Some(ref cc) = consumer_conn {
                let _ = cc.sender.send(settled_frame);
            }

            state.record_wallet_event(&session.consumer_principal, "settlement", -(session.total_msat as i64), "settled");
            state.record_wallet_event(&session.provider_principal, "settlement", session.provider_msat as i64, "settled");
            
            state.remove_session(&session_id);
            Ok(())
        }
        Frame::Cancel { session_id } => {
            if let Some(session) = state.get_session(&session_id) {
                let conn_info = match state.get_connection(connection_id) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                let sender_principal = conn_info.principal.as_deref().unwrap_or(charon_core::auth::NutsAuth::dev_principal());
                
                if sender_principal == session.consumer_principal || sender_principal == session.provider_principal {
                    let target_id = if connection_id == session.consumer_connection_id {
                        session.provider_connection_id
                    } else {
                        session.consumer_connection_id
                    };
                    
                    if let Some(target_conn) = state.get_connection(target_id) {
                        let _ = target_conn.sender.send(Frame::Cancel { session_id: session_id.clone() });
                    }
                    
                    state.remove_session(&session_id);
                } else {
                    let _ = tx.send(Frame::Error {
                        session_id: Some(session_id.clone()),
                        code: ErrorCode::AuthFailed,
                        message: "Sender not authorized for this session".into(),
                        http_status: Some(403),
                    });
                }
            }
            Ok(())
        }
        _ => {
            tracing::warn!(?frame, "Received unexpected frame from client");
            Ok(())
        }
    }
}

async fn relay_opaque(
    state: &Arc<GatewayState>,
    connection_id: Uuid,
    session_id: String,
    mut inner_frame: Frame,
) {
    let session = match state.get_session(&session_id) {
        Some(s) => s,
        None => {
            let _ = state.send_to_connection(connection_id, Frame::Error {
                session_id: Some(session_id.clone()),
                code: ErrorCode::AuthFailed,
                message: "Session not found".into(),
                http_status: Some(404),
            });
            return;
        }
    };
    
    if connection_id != session.consumer_connection_id && connection_id != session.provider_connection_id {
        let _ = state.send_to_connection(connection_id, Frame::Error {
            session_id: Some(session_id.clone()),
            code: ErrorCode::AuthFailed,
            message: "Sender not authorized for this session".into(),
            http_status: Some(403),
        });
        return;
    }
    
    let recipient_id = if connection_id == session.consumer_connection_id {
        session.provider_connection_id
    } else {
        session.consumer_connection_id
    };
    
    match &mut inner_frame {
        Frame::Hs { session_id: ref mut s, .. } => *s = session_id.clone(),
        Frame::Req { session_id: ref mut s, .. } => *s = session_id.clone(),
        Frame::ResHead { session_id: ref mut s, .. } => *s = session_id.clone(),
        Frame::Res { session_id: ref mut s, .. } => *s = session_id.clone(),
        _ => {}
    }
    
    let deliver_frame = Frame::Deliver {
        session_id: session_id.clone(),
        frame: Box::new(inner_frame),
    };
    
    if let Some(target_conn) = state.get_connection(recipient_id) {
        if let Err(_) = target_conn.sender.send(deliver_frame) {
            let _ = state.send_to_connection(connection_id, Frame::Error {
                session_id: Some(session_id.clone()),
                code: ErrorCode::ProviderGone,
                message: "Peer disconnected".into(),
                http_status: Some(503),
            });
        }
    } else {
        let _ = state.send_to_connection(connection_id, Frame::Error {
            session_id: Some(session_id.clone()),
            code: ErrorCode::ProviderGone,
            message: "Peer is not connected".into(),
            http_status: Some(503),
        });
    }
}

pub async fn run_server(
    state: Arc<GatewayState>,
    listener: TcpListener,
) -> anyhow::Result<()> {
    use axum::http::{HeaderValue, Method, header::{AUTHORIZATION, CONTENT_TYPE}};
    use tower_http::cors::CorsLayer;

    let origins_str = std::env::var("CHARON_CORS_ORIGINS").unwrap_or_else(|_| {
        "https://dashboard.charon.nuts.services,http://localhost:5173,http://localhost:3000".to_string()
    });

    let mut allowed_origins = Vec::new();
    for origin in origins_str.split(',') {
        let trimmed = origin.trim();
        if !trimmed.is_empty() {
            if let Ok(val) = trimmed.parse::<HeaderValue>() {
                allowed_origins.push(val);
            }
        }
    }

    let cors_layer = CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE]);

    let v1_router = Router::new()
        .route("/directory", get(get_directory))
        .route("/providers/{principal}/reputation", get(get_reputation))
        .route("/quote", post(post_quote))
        .route("/wallet/deposit", post(wallet_deposit))
        .route("/wallet/balance", get(wallet_balance))
        .route("/wallet/history", get(wallet_history))
        .route("/ratings", post(post_ratings))
        .layer(cors_layer);

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .nest("/v1", v1_router)
        .with_state(state)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();

    axum::serve(listener, app).await?;
    Ok(())
}
