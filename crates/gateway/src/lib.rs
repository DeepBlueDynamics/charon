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
use firestore::{paths, FirestoreDb};

/// Starter balance (msat) for a brand-new principal. **0 in production** — set
/// CHARON_DEV_BALANCE_MSAT to seed dev/test wallets before a real mint exists.
fn dev_balance_msat() -> u64 {
    std::env::var("CHARON_DEV_BALANCE_MSAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
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
    fn verify_payment<'a>(
        &'a self,
        payment: &'a charon_core::wire::Payment,
        expected_total_msat: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ErrorCode>> + Send + 'a>>;
}

pub struct DevPaymentVerifier;

impl PaymentVerifier for DevPaymentVerifier {
    fn verify_payment<'a>(
        &'a self,
        _payment: &'a charon_core::wire::Payment,
        _expected_total_msat: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ErrorCode>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }
}

pub struct CashuVerifier {
    pub allowlist: HashSet<String>,
    pub mock_amount: Option<u64>,
}

impl CashuVerifier {
    pub fn new(allowlist: HashSet<String>) -> Self {
        Self {
            allowlist,
            mock_amount: None,
        }
    }

    pub fn new_with_mock(allowlist: HashSet<String>, mock_amount: u64) -> Self {
        Self {
            allowlist,
            mock_amount: Some(mock_amount),
        }
    }
}

impl PaymentVerifier for CashuVerifier {
    fn verify_payment<'a>(
        &'a self,
        payment: &'a charon_core::wire::Payment,
        expected_total_msat: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ErrorCode>> + Send + 'a>> {
        Box::pin(async move {
            match payment {
                charon_core::wire::Payment::Cashu { token: token_str } => {
                    use std::str::FromStr;
                    // 1. parse cashuB token with cdk
                    let token = cdk::nuts::nut00::Token::from_str(token_str)
                        .map_err(|e| {
                            tracing::error!("Failed to parse Cashu token: {:?}", e);
                            ErrorCode::PaymentRequired
                        })?;

                    // 2. confirm mint URL is allowlisted
                    let mint_url = token.mint_url().map_err(|e| {
                        tracing::error!("Failed to get mint URL from token: {:?}", e);
                        ErrorCode::PaymentRequired
                    })?;
                    let mint_url_str = mint_url.to_string();
                    if !self.allowlist.contains(&mint_url_str) {
                        tracing::warn!("Mint URL {} is not allowlisted", mint_url_str);
                        return Err(ErrorCode::PaymentRequired);
                    }

                    // 3. redeem/swap proofs via cdk
                    let amount_msat = if let Some(mock_val) = self.mock_amount {
                        mock_val
                    } else {
                        let mut seed = [0u8; 64];
                        for chunk in seed.chunks_mut(16) {
                            chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                        }

                        let localstore = cdk_sqlite::wallet::memory::empty().await
                            .map_err(|e| {
                                tracing::error!("Failed to initialize in-memory wallet database: {:?}", e);
                                ErrorCode::PaymentRequired
                            })?;

                        let unit = token.unit().unwrap_or(cdk::nuts::CurrencyUnit::Sat);
                        let wallet = cdk::wallet::Wallet::new(
                            &mint_url_str,
                            unit.clone(),
                            Arc::new(localstore),
                            seed,
                            None,
                        ).map_err(|e| {
                            tracing::error!("Failed to initialize wallet: {:?}", e);
                            ErrorCode::PaymentRequired
                        })?;

                        // Claim/swap the token
                        let amount = wallet.receive(token_str, cdk::wallet::ReceiveOptions::default()).await
                            .map_err(|e| {
                                tracing::error!("Cashu redeem/swap failed: {:?}", e);
                                ErrorCode::PaymentRequired
                            })?;

                        amount.with_unit(unit).to_msat().map_err(|e| {
                            tracing::error!("Failed to convert amount to msat: {:?}", e);
                            ErrorCode::PaymentRequired
                        })?
                    };

                    if amount_msat < expected_total_msat {
                        tracing::warn!("Underpaid: expected {}, received {}", expected_total_msat, amount_msat);
                        return Err(ErrorCode::Underpaid);
                    }

                    Ok(())
                }
                _ => {
                    tracing::warn!("Payment rail not supported in production mode");
                    Err(ErrorCode::PaymentRequired)
                }
            }
        })
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalletDoc {
    pub balance_msat: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettlementDoc {
    pub session_id: String,
    pub consumer: String,
    pub provider: String,
    pub total_msat: u64,
    pub gateway_msat: u64,
    pub provider_msat: u64,
    pub outcome: String,
    pub ts: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RatingDoc {
    pub rating_id: String,
    pub provider: String,
    pub rater: String,
    pub session_id: String,
    pub score: u8,
    pub settled_msat: u64,
    pub rating_json: String,
    pub ts: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderDoc {
    pub principal: String,
    pub handle: String,
    pub models: Vec<ModelCard>,
    pub keybind: Keybind,
    pub payout: Payout,
    pub connection_id: String,
    pub last_seen: u64,
}

pub type StoreResult<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + Send + 'a>>;

pub trait Store: Send + Sync {
    fn get_balance<'a>(&'a self, principal: &'a str) -> StoreResult<'a, u64>;
    fn record_wallet_event<'a>(&'a self, principal: &'a str, kind: &'a str, amount_msat: i64, status: &'a str) -> StoreResult<'a, ()>;
    fn get_wallet_history<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Vec<WalletEntry>>;
    fn record_settlement<'a>(&'a self, session_id: &'a str, consumer: &'a str, provider: &'a str, total_msat: u64, gateway_msat: u64, provider_msat: u64, outcome: &'a str) -> StoreResult<'a, ()>;
    fn register_provider(&self, provider: ProviderConnection) -> StoreResult<'_, ()>;
    fn update_provider_heartbeat<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()>;
    fn get_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Option<ProviderConnection>>;
    fn find_provider_by_handle<'a>(&'a self, handle: &'a str) -> StoreResult<'a, Option<ProviderConnection>>;
    fn get_active_providers(&self) -> StoreResult<'_, Vec<ProviderConnection>>;
    fn remove_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()>;
    fn add_rating(&self, rating: serde_json::Value) -> StoreResult<'_, ()>;
    fn get_reputation<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ReputationResponse>;
    fn upload_receipt<'a>(&'a self, session_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()>;
    fn upload_attestation<'a>(&'a self, rating_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()>;
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct InMemoryProvider {
    pub provider: ProviderConnection,
    pub last_seen: u64,
}

pub struct InMemoryStore {
    pub wallets: Mutex<HashMap<String, UserWallet>>,
    pub settlements: Mutex<HashMap<String, SettlementDoc>>,
    pub providers: Mutex<HashMap<String, InMemoryProvider>>,
    pub ratings: Mutex<Vec<serde_json::Value>>,
    pub gcs_blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            wallets: Mutex::new(HashMap::new()),
            settlements: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
            ratings: Mutex::new(Vec::new()),
            gcs_blobs: Mutex::new(HashMap::new()),
        }
    }
}

impl Store for InMemoryStore {
    fn get_balance<'a>(&'a self, principal: &'a str) -> StoreResult<'a, u64> {
        Box::pin(async move {
            let mut wallets = self.wallets.lock().unwrap();
            let wallet = wallets.entry(principal.to_string()).or_insert_with(|| UserWallet {
                balance_msat: dev_balance_msat(),
                history: Vec::new(),
            });
            Ok(wallet.balance_msat)
        })
    }

    fn record_wallet_event<'a>(&'a self, principal: &'a str, kind: &'a str, amount_msat: i64, status: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
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
                balance_msat: dev_balance_msat(),
                history: Vec::new(),
            });

            if amount_msat >= 0 {
                wallet.balance_msat = wallet.balance_msat.saturating_add(amount_msat as u64);
            } else {
                let amount_abs = amount_msat.unsigned_abs();
                wallet.balance_msat = wallet.balance_msat.saturating_sub(amount_abs);
            }

            wallet.history.push(entry);
            Ok(())
        })
    }

    fn get_wallet_history<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Vec<WalletEntry>> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let cutoff = now.saturating_sub(14 * 24 * 60 * 60);

            let mut wallets = self.wallets.lock().unwrap();
            let wallet = wallets.entry(principal.to_string()).or_insert_with(|| UserWallet {
                balance_msat: dev_balance_msat(),
                history: Vec::new(),
            });

            wallet.history.retain(|e| e.ts >= cutoff);
            Ok(wallet.history.clone())
        })
    }

    fn record_settlement<'a>(&'a self, session_id: &'a str, consumer: &'a str, provider: &'a str, total_msat: u64, gateway_msat: u64, provider_msat: u64, outcome: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let doc = SettlementDoc {
                session_id: session_id.to_string(),
                consumer: consumer.to_string(),
                provider: provider.to_string(),
                total_msat,
                gateway_msat,
                provider_msat,
                outcome: outcome.to_string(),
                ts,
            };

            self.settlements.lock().unwrap().insert(session_id.to_string(), doc);
            Ok(())
        })
    }

    fn register_provider(&self, provider: ProviderConnection) -> StoreResult<'_, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let principal = provider.principal.clone();
            self.providers.lock().unwrap().insert(principal, InMemoryProvider {
                provider,
                last_seen: ts,
            });
            Ok(())
        })
    }

    fn update_provider_heartbeat<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let mut providers = self.providers.lock().unwrap();
            if let Some(entry) = providers.get_mut(principal) {
                entry.last_seen = ts;
            }
            Ok(())
        })
    }

    fn get_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Option<ProviderConnection>> {
        Box::pin(async move {
            let providers = self.providers.lock().unwrap();
            Ok(providers.get(principal).map(|p| p.provider.clone()))
        })
    }

    fn find_provider_by_handle<'a>(&'a self, handle: &'a str) -> StoreResult<'a, Option<ProviderConnection>> {
        Box::pin(async move {
            let providers = self.providers.lock().unwrap();
            let found = providers.values()
                .find(|p| provider_handle(&p.provider.principal) == handle)
                .map(|p| p.provider.clone());
            Ok(found)
        })
    }

    fn get_active_providers(&self) -> StoreResult<'_, Vec<ProviderConnection>> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let threshold = now.saturating_sub(90);

            let providers = self.providers.lock().unwrap();
            let list: Vec<ProviderConnection> = providers.values()
                .filter(|p| p.last_seen >= threshold)
                .map(|p| p.provider.clone())
                .collect();
            Ok(list)
        })
    }

    fn remove_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            self.providers.lock().unwrap().remove(principal);
            Ok(())
        })
    }

    fn add_rating(&self, rating: serde_json::Value) -> StoreResult<'_, ()> {
        Box::pin(async move {
            self.ratings.lock().unwrap().push(rating);
            Ok(())
        })
    }

    fn get_reputation<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ReputationResponse> {
        Box::pin(async move {
            let ratings_list = self.ratings.lock().unwrap();
            let mut matching_ratings = Vec::new();
            let mut sum_score_weight = 0.0;
            let mut sum_weight = 0.0;

            for r in ratings_list.iter() {
                if let Some(subject) = r.get("subject").and_then(|v| v.as_str()) {
                    if subject == principal {
                        matching_ratings.push(r.clone());
                        let score = r.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let settled_msat = r.get("settled_msat").and_then(|v| v.as_u64()).unwrap_or(0);
                        if settled_msat > 0 {
                            sum_score_weight += score * (settled_msat as f64);
                            sum_weight += settled_msat as f64;
                        }
                    }
                }
            }

            let average_score = if sum_weight > 0.0 {
                sum_score_weight / sum_weight
            } else {
                0.0
            };

            let settlements = self.settlements.lock().unwrap();
            let mut total_settled_msat = 0;
            for s in settlements.values() {
                if s.provider == principal {
                    total_settled_msat += s.total_msat;
                }
            }

            Ok(ReputationResponse {
                ratings: matching_ratings,
                average_score,
                total_settled_msat,
            })
        })
    }

    fn upload_receipt<'a>(&'a self, session_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()> {
        Box::pin(async move {
            self.gcs_blobs.lock().unwrap().insert(format!("receipts/{}.json", session_id), data.to_vec());
            Ok(())
        })
    }

    fn upload_attestation<'a>(&'a self, rating_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()> {
        Box::pin(async move {
            self.gcs_blobs.lock().unwrap().insert(format!("attestations/{}.json", rating_id), data.to_vec());
            Ok(())
        })
    }
}

pub struct CloudStore {
    pub db: firestore::FirestoreDb,
    pub gcs: google_cloud_storage::client::Client,
    pub bucket: String,
}

impl CloudStore {
    pub fn new(db: firestore::FirestoreDb, gcs: google_cloud_storage::client::Client, bucket: String) -> Self {
        Self { db, gcs, bucket }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProviderHeartbeatUpdate {
    last_seen: u64,
}

impl Store for CloudStore {
    fn get_balance<'a>(&'a self, principal: &'a str) -> StoreResult<'a, u64> {
        Box::pin(async move {
            let doc_opt: Option<WalletDoc> = self.db.fluent()
                .select()
                .by_id_in("wallets")
                .obj()
                .one(principal)
                .await?;
            Ok(doc_opt.map(|d| d.balance_msat).unwrap_or_else(dev_balance_msat))
        })
    }

    fn record_wallet_event<'a>(&'a self, principal: &'a str, kind: &'a str, amount_msat: i64, status: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            let old_balance = self.get_balance(principal).await?;
            let new_balance = if amount_msat >= 0 {
                old_balance.saturating_add(amount_msat as u64)
            } else {
                old_balance.saturating_sub(amount_msat.unsigned_abs())
            };

            self.db.fluent()
                .insert()
                .into("wallets")
                .document_id(principal)
                .object(&WalletDoc { balance_msat: new_balance })
                .execute::<()>()
                .await?;

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

            let entry_id = format!("{}_{}", ts, uuid::Uuid::new_v4());
            self.db.fluent()
                .insert()
                .into(format!("wallets/{}/history", principal).as_str())
                .document_id(&entry_id)
                .object(&entry)
                .execute::<()>()
                .await?;

            Ok(())
        })
    }

    fn get_wallet_history<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Vec<WalletEntry>> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let cutoff = now.saturating_sub(14 * 24 * 60 * 60);

            let list: Vec<WalletEntry> = self.db.fluent()
                .select()
                .from(format!("wallets/{}/history", principal).as_str())
                .filter(|q| q.field("ts").greater_than_or_equal(cutoff))
                .obj()
                .query()
                .await?;
            Ok(list)
        })
    }

    fn record_settlement<'a>(&'a self, session_id: &'a str, consumer: &'a str, provider: &'a str, total_msat: u64, gateway_msat: u64, provider_msat: u64, outcome: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let doc = SettlementDoc {
                session_id: session_id.to_string(),
                consumer: consumer.to_string(),
                provider: provider.to_string(),
                total_msat,
                gateway_msat,
                provider_msat,
                outcome: outcome.to_string(),
                ts,
            };

            self.db.fluent()
                .insert()
                .into("settlements")
                .document_id(session_id)
                .object(&doc)
                .execute::<()>()
                .await?;

            Ok(())
        })
    }

    fn register_provider(&self, provider: ProviderConnection) -> StoreResult<'_, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let principal = provider.principal.clone();
            let handle = provider_handle(&principal);
            let doc = ProviderDoc {
                principal,
                handle,
                models: provider.models,
                keybind: provider.keybind,
                payout: provider.payout,
                connection_id: provider.connection_id.to_string(),
                last_seen: ts,
            };

            self.db.fluent()
                .insert()
                .into("providers")
                .document_id(&doc.principal)
                .object(&doc)
                .execute::<()>()
                .await?;

            Ok(())
        })
    }

    fn update_provider_heartbeat<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            self.db.fluent()
                .update()
                .fields(paths!(ProviderDoc::last_seen))
                .in_col("providers")
                .document_id(principal)
                .object(&ProviderHeartbeatUpdate { last_seen: ts })
                .execute::<()>()
                .await?;

            Ok(())
        })
    }

    fn get_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, Option<ProviderConnection>> {
        Box::pin(async move {
            let doc_opt: Option<ProviderDoc> = self.db.fluent()
                .select()
                .by_id_in("providers")
                .obj()
                .one(principal)
                .await?;

            if let Some(doc) = doc_opt {
                let conn_id = uuid::Uuid::parse_str(&doc.connection_id).unwrap_or_default();
                Ok(Some(ProviderConnection {
                    principal: doc.principal,
                    models: doc.models,
                    keybind: doc.keybind,
                    payout: doc.payout,
                    connection_id: conn_id,
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn find_provider_by_handle<'a>(&'a self, handle: &'a str) -> StoreResult<'a, Option<ProviderConnection>> {
        Box::pin(async move {
            let list: Vec<ProviderDoc> = self.db.fluent()
                .select()
                .from("providers")
                .filter(|q| q.field("handle").equal(handle))
                .obj()
                .query()
                .await?;

            if let Some(doc) = list.into_iter().next() {
                let conn_id = uuid::Uuid::parse_str(&doc.connection_id).unwrap_or_default();
                Ok(Some(ProviderConnection {
                    principal: doc.principal,
                    models: doc.models,
                    keybind: doc.keybind,
                    payout: doc.payout,
                    connection_id: conn_id,
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn get_active_providers(&self) -> StoreResult<'_, Vec<ProviderConnection>> {
        Box::pin(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let threshold = now.saturating_sub(90);

            let list: Vec<ProviderDoc> = self.db.fluent()
                .select()
                .from("providers")
                .filter(|q| q.field("last_seen").greater_than_or_equal(threshold))
                .obj()
                .query()
                .await?;

            let result = list.into_iter()
                .map(|doc| {
                    let conn_id = uuid::Uuid::parse_str(&doc.connection_id).unwrap_or_default();
                    ProviderConnection {
                        principal: doc.principal,
                        models: doc.models,
                        keybind: doc.keybind,
                        payout: doc.payout,
                        connection_id: conn_id,
                    }
                })
                .collect();
            Ok(result)
        })
    }

    fn remove_provider<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ()> {
        Box::pin(async move {
            self.db.fluent()
                .delete()
                .from("providers")
                .document_id(principal)
                .execute()
                .await?;
            Ok(())
        })
    }

    fn add_rating(&self, rating: serde_json::Value) -> StoreResult<'_, ()> {
        Box::pin(async move {
            let session_id = rating.get("session_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let rating_id = if session_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                session_id.clone()
            };

            let provider = rating.get("subject").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let rater = rating.get("rater").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let score = rating.get("score").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let settled_msat = rating.get("settled_msat").and_then(|v| v.as_u64()).unwrap_or(0);
            let ts = rating.get("ts").and_then(|v| v.as_u64()).unwrap_or_default();

            let doc = RatingDoc {
                rating_id: rating_id.clone(),
                provider,
                rater,
                session_id,
                score,
                settled_msat,
                rating_json: rating.to_string(),
                ts,
            };

            self.db.fluent()
                .insert()
                .into("ratings")
                .document_id(&rating_id)
                .object(&doc)
                .execute::<()>()
                .await?;
            Ok(())
        })
    }

    fn get_reputation<'a>(&'a self, principal: &'a str) -> StoreResult<'a, ReputationResponse> {
        Box::pin(async move {
            let list: Vec<RatingDoc> = self.db.fluent()
                .select()
                .from("ratings")
                .filter(|q| q.field("provider").equal(principal))
                .obj()
                .query()
                .await?;

            let mut ratings = Vec::new();
            let mut sum_score_weight = 0.0;
            let mut sum_weight = 0.0;

            for doc in list {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&doc.rating_json) {
                    ratings.push(val);
                }
                if doc.settled_msat > 0 {
                    sum_score_weight += (doc.score as f64) * (doc.settled_msat as f64);
                    sum_weight += doc.settled_msat as f64;
                }
            }

            let average_score = if sum_weight > 0.0 {
                sum_score_weight / sum_weight
            } else {
                0.0
            };

            let settlements: Vec<SettlementDoc> = self.db.fluent()
                .select()
                .from("settlements")
                .filter(|q| q.field("provider").equal(principal))
                .obj()
                .query()
                .await?;

            let total_settled_msat = settlements.iter().map(|s| s.total_msat).sum();

            Ok(ReputationResponse {
                ratings,
                average_score,
                total_settled_msat,
            })
        })
    }

    fn upload_receipt<'a>(&'a self, session_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
            let object_name = format!("receipts/{}.json", session_id);
            let upload_type = UploadType::Simple(Media::new(object_name));
            let request = UploadObjectRequest {
                bucket: self.bucket.clone(),
                ..Default::default()
            };
            self.gcs.upload_object(&request, data.to_vec(), &upload_type).await?;
            Ok(())
        })
    }

    fn upload_attestation<'a>(&'a self, rating_id: &'a str, data: &'a [u8]) -> StoreResult<'a, ()> {
        Box::pin(async move {
            use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
            let object_name = format!("attestations/{}.json", rating_id);
            let upload_type = UploadType::Simple(Media::new(object_name));
            let request = UploadObjectRequest {
                bucket: self.bucket.clone(),
                ..Default::default()
            };
            self.gcs.upload_object(&request, data.to_vec(), &upload_type).await?;
            Ok(())
        })
    }
}

pub async fn detect_store() -> Arc<dyn Store> {
    let project_id = std::env::var("GOOGLE_CLOUD_PROJECT").unwrap_or_else(|_| "gnosis-459403".to_string());
    let bucket_opt = std::env::var("CHARON_GCS_BUCKET").ok();
    
    if let Some(bucket) = bucket_opt {
        match google_cloud_storage::client::ClientConfig::default().with_auth().await {
            Ok(gcs_config) => {
                let gcs_client = google_cloud_storage::client::Client::new(gcs_config);
                match FirestoreDb::new(&project_id).await {
                    Ok(firestore_db) => {
                        tracing::info!("Successfully initialized Google Cloud Store (Firestore project={}, GCS bucket={})", project_id, bucket);
                        return Arc::new(CloudStore::new(firestore_db, gcs_client, bucket));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to initialize Firestore client: {:?}. Falling back to InMemoryStore.", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to initialize GCS ClientConfig with auth: {:?}. Falling back to InMemoryStore.", e);
            }
        }
    } else {
        tracing::info!("CHARON_GCS_BUCKET not set. Using InMemoryStore.");
    }
    
    Arc::new(InMemoryStore::new())
}

pub struct GatewayState {
    pub store: Arc<dyn Store>,
    pub sessions: Mutex<HashMap<String, SessionInfo>>,
    pub principal_sessions: Mutex<HashMap<String, HashSet<String>>>,
    pub connections: Mutex<HashMap<Uuid, ConnectionInfo>>,
    pub rate_limits: Mutex<HashMap<String, Vec<tokio::time::Instant>>>,
    pub authenticator: Arc<dyn Authenticator>,
    pub payment_verifier: Arc<dyn PaymentVerifier>,
    pub disable_auth: bool,
    pub markup_bps: u64,
    pub floor_msat: u64,
}

impl GatewayState {
    pub fn new(
        store: Arc<dyn Store>,
        authenticator: Arc<dyn Authenticator>,
        payment_verifier: Arc<dyn PaymentVerifier>,
        disable_auth: bool,
        markup_bps: u64,
        floor_msat: u64,
    ) -> Self {
        Self {
            store,
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

    pub async fn record_wallet_event(&self, principal: &str, kind: &str, amount_msat: i64, status: &str) {
        if let Err(e) = self.store.record_wallet_event(principal, kind, amount_msat, status).await {
            tracing::error!("Failed to record wallet event: {:?}", e);
        }
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

    pub async fn remove_connection(&self, id: Uuid) {
        let principal_opt = {
            let mut connections = self.connections.lock().unwrap();
            connections.remove(&id).and_then(|c| c.principal)
        };

        if let Some(ref principal) = principal_opt {
            if let Ok(Some(p)) = self.store.get_provider(principal).await {
                if p.connection_id == id {
                    let _ = self.store.remove_provider(principal).await;
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

    pub async fn register_provider(&self, principal: String, models: Vec<ModelCard>, keybind: Keybind, payout: Payout, connection_id: Uuid) -> anyhow::Result<()> {
        let old_conn_id = {
            let old_p = self.store.get_provider(&principal).await?;
            old_p.map(|p| p.connection_id)
        };

        self.store.register_provider(ProviderConnection {
            principal,
            models,
            keybind,
            payout,
            connection_id,
        }).await?;

        if let Some(old_id) = old_conn_id {
            if old_id != connection_id {
                self.remove_connection(old_id).await;
            }
        }
        Ok(())
    }

    pub async fn get_provider(&self, principal: &str) -> Option<ProviderConnection> {
        self.store.get_provider(principal).await.ok().flatten()
    }

    pub async fn find_provider_by_handle(&self, handle: &str) -> Option<ProviderConnection> {
        self.store.find_provider_by_handle(handle).await.ok().flatten()
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

pub fn provider_handle(principal: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(principal.as_bytes());
    let hex_str: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    format!("charon:{}", &hex_str[..12])
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
    let active_providers = state.store.get_active_providers().await.unwrap_or_default();
    let entries: Vec<DirectoryEntry> = active_providers
        .into_iter()
        .map(|p| DirectoryEntry {
            principal: provider_handle(&p.principal),
            models: p.models,
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
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    axum::extract::Path(principal): axum::extract::Path<String>,
    _principal: HttpPrincipal,
) -> Result<axum::Json<ReputationResponse>, (StatusCode, String)> {
    let real_principal = if principal.starts_with("charon:") {
        match state.store.find_provider_by_handle(&principal).await {
            Ok(Some(p)) => p.principal,
            _ => return Err((StatusCode::NOT_FOUND, "Provider not found".to_string())),
        }
    } else {
        principal
    };

    match state.store.get_reputation(&real_principal).await {
        Ok(rep) => Ok(axum::Json(rep)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get reputation: {:?}", e))),
    }
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
    let providers = state.store.get_active_providers().await.unwrap_or_default();
    let mut rate_opt = None;
    for p in providers {
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
) -> Result<axum::Json<BalanceResponse>, (StatusCode, String)> {
    match state.store.get_balance(&principal.0).await {
        Ok(balance_msat) => Ok(axum::Json(BalanceResponse { balance_msat })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get balance: {:?}", e))),
    }
}

#[derive(serde::Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<WalletEntry>,
}

pub async fn wallet_history(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    principal: HttpPrincipal,
) -> Result<axum::Json<HistoryResponse>, (StatusCode, String)> {
    match state.store.get_wallet_history(&principal.0).await {
        Ok(entries) => Ok(axum::Json(HistoryResponse { entries })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get history: {:?}", e))),
    }
}

pub async fn post_ratings(
    axum::extract::State(state): axum::extract::State<Arc<GatewayState>>,
    _principal: HttpPrincipal,
    axum::Json(rating): axum::Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session_id = rating.get("session_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    if session_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Missing session_id".to_string()));
    }
    
    let rating_id = session_id.clone();
    
    let data = serde_json::to_vec(&rating).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Err(e) = state.store.upload_attestation(&rating_id, &data).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to upload attestation to GCS: {:?}", e)));
    }
    
    if let Err(e) = state.store.add_rating(rating).await {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to save rating to Firestore: {:?}", e)));
    }
    
    Ok(StatusCode::OK)
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
                    if let Some(conn) = reader_state.get_connection(reader_conn_id) {
                        if let Some(ref principal) = conn.principal {
                            let _ = reader_state.store.update_provider_heartbeat(principal).await;
                        }
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
    state.remove_connection(connection_id).await;
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
                    
                    let _ = state.register_provider(
                        principal.clone(),
                        models,
                        keybind,
                        payout,
                        connection_id,
                    ).await;
                    
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
            
            let provider = match state.get_provider(&envelope.provider).await {
                Some(p) => Some(p),
                None => state.find_provider_by_handle(&envelope.provider).await,
            };
            
            let provider = match provider {
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
            
            if let Err(err_code) = state.payment_verifier.verify_payment(&envelope.payment, quote.total_msat).await {
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
 
            // Record Cashu settlement split (gateway keeps gateway_msat, provider credited provider_msat)
            // TODO: Provider P2PK payout + change/refund are follow-ups.
            state.record_wallet_event("gateway", "cashu_fee", quote.gateway_msat as i64, "settled").await;
            state.record_wallet_event(&provider.principal, "cashu_credit", quote.provider_msat as i64, "settled").await;
            
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

            state.record_wallet_event(&session.consumer_principal, "settlement", -(session.total_msat as i64), "settled").await;
            state.record_wallet_event(&session.provider_principal, "settlement", session.provider_msat as i64, "settled").await;
            
            let _ = state.store.record_settlement(
                &session_id,
                &session.consumer_principal,
                &session.provider_principal,
                session.total_msat,
                session.gateway_msat,
                session.provider_msat,
                "ok",
            ).await;

            let receipt = serde_json::json!({
                "session_id": session_id.clone(),
                "consumer": session.consumer_principal.clone(),
                "provider": session.provider_principal.clone(),
                "total_msat": session.total_msat,
                "gateway_msat": session.gateway_msat,
                "provider_msat": session.provider_msat,
                "outcome": "ok",
                "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
            });
            if let Ok(receipt_bytes) = serde_json::to_vec(&receipt) {
                let _ = state.store.upload_receipt(&session_id, &receipt_bytes).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use charon_core::wire::Payment;
    use std::collections::HashSet;

    const MOCK_TOKEN: &str = "cashuAeyJ0b2tlbiI6W3sicHJvb2ZzIjpbeyJhbW91bnQiOjEsInNlY3JldCI6ImI0ZjVlNDAxMDJhMzhiYjg3NDNiOTkwMzU5MTU1MGYyZGEzZTQxNWEzMzU0OTUyN2M2MmM5ZDc5MGVmYjM3MDUiLCJDIjoiMDIzYmU1M2U4YzYwNTMwZWVhOWIzOTQzZmRhMWEyY2U3MWM3YjNmMGNmMGRjNmQ4NDZmYTc2NWFhZjc3OWZhODFkIiwiaWQiOiIwMDlhMWYyOTMyNTNlNDFlIn1dLCJtaW50IjoiaHR0cHM6Ly90ZXN0bnV0LmNhc2h1LnNwYWNlIn1dLCJ1bml0Ijoic2F0In0=";

    #[tokio::test]
    async fn test_cashu_verifier_allowlist_rejection() {
        let verifier = CashuVerifier::new_with_mock(HashSet::new(), 1000);
        let payment = Payment::Cashu { token: MOCK_TOKEN.to_string() };
        let res = verifier.verify_payment(&payment, 1000).await;
        assert_eq!(res, Err(ErrorCode::PaymentRequired));
    }

    #[tokio::test]
    async fn test_cashu_verifier_allowlist_acceptance_but_underpaid() {
        let mut allowlist = HashSet::new();
        allowlist.insert("https://testnut.cashu.space".to_string());
        
        let verifier = CashuVerifier::new_with_mock(allowlist, 500);
        let payment = Payment::Cashu { token: MOCK_TOKEN.to_string() };
        let res = verifier.verify_payment(&payment, 1000).await;
        assert_eq!(res, Err(ErrorCode::Underpaid));
    }

    #[tokio::test]
    async fn test_cashu_verifier_success() {
        let mut allowlist = HashSet::new();
        allowlist.insert("https://testnut.cashu.space".to_string());
        
        let verifier = CashuVerifier::new_with_mock(allowlist, 1000);
        let payment = Payment::Cashu { token: MOCK_TOKEN.to_string() };
        let res = verifier.verify_payment(&payment, 1000).await;
        assert_eq!(res, Ok(()));
    }

    #[tokio::test]
    #[ignore]
    async fn test_cashu_verifier_live_mint_redemption() {
        let mut allowlist = HashSet::new();
        allowlist.insert("https://testnut.cashu.space".to_string());
        
        let verifier = CashuVerifier::new(allowlist);
        let payment = Payment::Cashu { token: MOCK_TOKEN.to_string() };
        let res = verifier.verify_payment(&payment, 1000).await;
        assert_eq!(res, Err(ErrorCode::PaymentRequired));
    }

    #[tokio::test]
    async fn test_provider_pseudonymous_handle() {
        let email = "provider@example.com";
        let handle = provider_handle(email);

        // 1. Assert handle format (contains charon: and no @)
        assert!(handle.starts_with("charon:"));
        assert!(!handle.contains('@'));

        // 2. Setup gateway state
        let auth = Arc::new(GnosisAuthenticator::new("http://auth".to_string(), true));
        let verifier = Arc::new(DevPaymentVerifier);
        let store = Arc::new(InMemoryStore::new());
        let state = Arc::new(GatewayState::new(store, auth, verifier, true, 0, 0));

        let models = vec![ModelCard {
            name: "test-model".to_string(),
            backend: "ollama".to_string(),
            context_length: 2048,
            price_msat_per_mtok_in: 10,
            price_msat_per_mtok_out: 10,
        }];
        let keybind = Keybind {
            x25519_pub: "key".to_string(),
            sig: "sig".to_string(),
            not_after: 0,
        };
        let payout = charon_core::wire::Payout {
            rail: "cashu".to_string(),
            address: "target".to_string(),
        };
        let connection_id = Uuid::new_v4();

        let _ = state.register_provider(email.to_string(), models, keybind, payout, connection_id).await;

        // 3. Verify handle resolution
        let resolved = state.find_provider_by_handle(&handle).await;
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().principal, email);

        // 4. Verify raw principal still works
        let resolved_raw = state.get_provider(email).await;
        assert!(resolved_raw.is_some());
        assert_eq!(resolved_raw.unwrap().principal, email);

        // 5. Test get_directory endpoint obfuscation
        let state_extractor = axum::extract::State(state.clone());
        let http_principal = HttpPrincipal(email.to_string());
        let res_json = get_directory(state_extractor, http_principal).await;
        let entries = res_json.0;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].principal, handle);
        assert!(!entries[0].principal.contains('@'));
    }
}

