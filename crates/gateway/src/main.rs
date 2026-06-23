//! charon-gateway — the blind relay / matchmaker (spec 09).
//!
//! Listen on `0.0.0.0:$PORT` (Cloud Run injects `PORT`; default 8080). See
//! `/spec/11-deployment.md` for the Cloud Run / WebSocket settings.

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;
use charon_core::{Frame, Envelope, ModelCard, Keybind, ErrorCode};
use charon_core::wire::Payout;
use charon_core::payment::Rate;

#[derive(Parser, Debug)]
#[command(name = "charon-gateway", version, about = "Charon blind relay")]
struct Args {
    /// Bind address. Cloud Run sets PORT; honor it.
    #[arg(long, env = "BIND", default_value = "0.0.0.0:8080")]
    bind: String,
    /// NUTS auth base URL.
    #[arg(long, env = "GNOSIS_AUTH_URL", default_value = "https://auth.nuts.services")]
    auth_url: String,
    /// Skip token validation — private/test deployments only (spec 02).
    #[arg(long, env = "DISABLE_AUTH", default_value_t = false)]
    disable_auth: bool,
    /// Gateway markup in basis points.
    #[arg(long, env = "MARKUP_BPS", default_value_t = charon_core::payment::DEFAULT_MARKUP_BPS)]
    markup_bps: u64,
    /// Gateway floor in msat.
    #[arg(long, env = "FLOOR_MSAT", default_value_t = charon_core::payment::DEFAULT_FLOOR_MSAT)]
    floor_msat: u64,
}

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
        // Accept everything in dev mode
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
            self.nuts.validate(token).await
                .map_err(|_| ErrorCode::AuthFailed)
        })
    }
}

#[derive(Clone)]
struct ProviderConnection {
    principal: String,
    models: Vec<ModelCard>,
    #[allow(dead_code)]
    keybind: Keybind,
    #[allow(dead_code)]
    payout: Payout,
    connection_id: Uuid,
}

#[derive(Clone)]
struct SessionInfo {
    session_id: String,
    consumer_principal: String,
    consumer_connection_id: Uuid,
    provider_principal: String,
    provider_connection_id: Uuid,
    #[allow(dead_code)]
    envelope: Envelope,
    total_msat: u64,
    provider_msat: u64,
    gateway_msat: u64,
}

#[derive(Clone)]
struct ConnectionInfo {
    #[allow(dead_code)]
    id: Uuid,
    principal: Option<String>,
    sender: tokio::sync::mpsc::UnboundedSender<Frame>,
    #[allow(dead_code)]
    ip: IpAddr,
}

struct GatewayState {
    providers: Mutex<HashMap<String, ProviderConnection>>,
    sessions: Mutex<HashMap<String, SessionInfo>>,
    principal_sessions: Mutex<HashMap<String, HashSet<String>>>,
    connections: Mutex<HashMap<Uuid, ConnectionInfo>>,
    rate_limits: Mutex<HashMap<String, Vec<tokio::time::Instant>>>,
    authenticator: Arc<dyn Authenticator>,
    payment_verifier: Arc<dyn PaymentVerifier>,
    disable_auth: bool,
    markup_bps: u64,
    floor_msat: u64,
}

impl GatewayState {
    fn new(
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
            authenticator,
            payment_verifier,
            disable_auth,
            markup_bps,
            floor_msat,
        }
    }

    fn check_ip_connect_rate_limit(&self, ip: IpAddr) -> bool {
        self.check_rate_limit(&format!("ip_connect:{}", ip), 30)
    }

    fn check_principal_register_rate_limit(&self, principal: &str) -> bool {
        self.check_rate_limit(&format!("register:{}", principal), 30)
    }

    fn check_consumer_open_rate_limit(&self, principal: &str) -> bool {
        self.check_rate_limit(&format!("open:{}", principal), 60)
    }

    fn check_rate_limit(&self, key: &str, limit: usize) -> bool {
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

    fn get_active_session_count(&self, principal: &str) -> usize {
        let principal_sessions = self.principal_sessions.lock().unwrap();
        principal_sessions.get(principal).map(|s| s.len()).unwrap_or(0)
    }

    fn add_connection(&self, id: Uuid, principal: Option<String>, sender: tokio::sync::mpsc::UnboundedSender<Frame>, ip: IpAddr) {
        let mut connections = self.connections.lock().unwrap();
        connections.insert(id, ConnectionInfo { id, principal, sender, ip });
    }

    fn get_connection(&self, id: Uuid) -> Option<ConnectionInfo> {
        let connections = self.connections.lock().unwrap();
        connections.get(&id).cloned()
    }

    fn update_connection_principal(&self, id: Uuid, principal: Option<String>) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(conn) = connections.get_mut(&id) {
            conn.principal = principal;
        }
    }

    fn send_to_connection(&self, id: Uuid, frame: Frame) -> bool {
        let connections = self.connections.lock().unwrap();
        if let Some(conn) = connections.get(&id) {
            conn.sender.send(frame).is_ok()
        } else {
            false
        }
    }

    fn remove_connection(&self, id: Uuid) {
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

    fn register_provider(&self, principal: String, models: Vec<ModelCard>, keybind: Keybind, payout: Payout, connection_id: Uuid) {
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

    fn get_provider(&self, principal: &str) -> Option<ProviderConnection> {
        let providers = self.providers.lock().unwrap();
        providers.get(principal).cloned()
    }

    fn add_session(&self, session: SessionInfo) {
        let session_id = session.session_id.clone();
        let consumer = session.consumer_principal.clone();
        let provider = session.provider_principal.clone();
        
        self.sessions.lock().unwrap().insert(session_id.clone(), session);
        
        let mut principal_sessions = self.principal_sessions.lock().unwrap();
        principal_sessions.entry(consumer).or_default().insert(session_id.clone());
        principal_sessions.entry(provider).or_default().insert(session_id.clone());
    }

    fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_id).cloned()
    }

    fn remove_session(&self, session_id: &str) {
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

    fn cancel_session(&self, session_id: &str) {
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

fn extract_token(req: &tokio_tungstenite::tungstenite::handshake::server::Request) -> Option<String> {
    if let Some(auth_val) = req.headers().get("Authorization") {
        if let Ok(auth_str) = auth_val.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
            return Some(auth_str.trim().to_string());
        }
    }
    
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                if key == "token" || key == "ahp_token" {
                    return Some(val.to_string());
                }
            }
        }
    }
    
    None
}

async fn handle_connection(
    state: Arc<GatewayState>,
    stream: TcpStream,
    addr: std::net::SocketAddr,
) {
    let ip = addr.ip();
    
    if !state.check_ip_connect_rate_limit(ip) {
        tracing::warn!(%ip, "Connection rate limited");
        let mut stream = stream;
        let response = "HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\n\r\nRate limit exceeded\n";
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }

    let mut extracted_token = None;
    let mut initial_principal = None;
    
    let ws_stream_res = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request, response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            extracted_token = extract_token(req);
            Ok(response)
        }
    ).await;

    let ws_stream = match ws_stream_res {
        Ok(ws) => ws,
        Err(e) => {
            tracing::error!(error = ?e, "WebSocket handshake failed");
            return;
        }
    };

    if let Some(token) = extracted_token {
        match state.authenticator.authenticate(&token).await {
            Ok(principal) => {
                tracing::info!(%principal, "Authenticated connection via handshake token");
                initial_principal = Some(principal);
            }
            Err(err_code) => {
                tracing::warn!(?err_code, "Authentication failed during handshake");
                let mut ws = ws_stream;
                let err_frame = Frame::Error {
                    session_id: None,
                    code: err_code,
                    message: "Authentication failed".into(),
                    http_status: Some(401),
                };
                if let Ok(msg) = serde_json::to_string(&err_frame) {
                    let _ = ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.into())).await;
                }
                let _ = ws.close(None).await;
                return;
            }
        }
    }

    let connection_id = Uuid::new_v4();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    
    state.add_connection(connection_id, initial_principal.clone(), tx.clone(), ip);
    tracing::info!(%connection_id, principal = ?initial_principal, "Connection established");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    let last_pong_received = Arc::new(std::sync::atomic::AtomicBool::new(true));
    
    let mut rx_task = rx;
    let ws_send_loop = async move {
        while let Some(frame) = rx_task.recv().await {
            if let Ok(json_str) = serde_json::to_string(&frame) {
                if let Err(e) = ws_sender.send(tokio_tungstenite::tungstenite::Message::Text(json_str.into())).await {
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
    ping_interval.tick().await; // Skip first immediate tick

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
                            if msg.is_text() {
                                if let Ok(text) = msg.to_text() {
                                    match serde_json::from_str::<Frame>(text) {
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
                            } else if msg.is_close() {
                                break;
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
        Frame::Open { session_id, envelope } => {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = Args::parse();
    if let Ok(port) = std::env::var("PORT") {
        args.bind = format!("0.0.0.0:{port}");
    }

    tracing::info!(bind = %args.bind, auth = %args.auth_url, disable_auth = %args.disable_auth, "charon-gateway starting");

    let authenticator = Arc::new(GnosisAuthenticator::new(args.auth_url.clone(), args.disable_auth));
    let payment_verifier = Arc::new(DevPaymentVerifier);

    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        args.disable_auth,
        args.markup_bps,
        args.floor_msat,
    ));

    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "Listening on address");

    while let Ok((stream, addr)) = listener.accept().await {
        let state_clone = state.clone();
        tokio::spawn(async move {
            handle_connection(state_clone, stream, addr).await;
        });
    }

    Ok(())
}
