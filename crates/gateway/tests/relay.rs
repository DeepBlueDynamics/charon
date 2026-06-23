use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use charon_core::{Frame, Envelope, ModelCard, Keybind, Payment, ErrorCode};
use charon_core::wire::Payout;
use charon_gateway::{GatewayState, GnosisAuthenticator, DevPaymentVerifier, run_server};

#[tokio::test]
async fn test_relay_flow_and_injection_prevention() {
    // 1. Bind and start the gateway server in-process on a random port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    
    let authenticator = Arc::new(GnosisAuthenticator::new("".to_string(), true));
    let payment_verifier = Arc::new(DevPaymentVerifier);
    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        true, // disable_auth
        1000, // markup_bps
        21000, // floor_msat
    ));
    
    let state_clone = state.clone();
    tokio::spawn(async move {
        run_server(state_clone, listener).await.unwrap();
    });

    // Let the server startup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 2. Connect the provider client and register
    let provider_url = format!("ws://127.0.0.1:{}/ws?token=ahp_provider_a", port);
    let (mut prov_ws, _) = connect_async(provider_url).await.unwrap();

    let register_frame = Frame::Register {
        ahp_token: "ahp_provider_a".to_string(),
        keybind: Keybind {
            x25519_pub: "pub_key_prov".to_string(),
            sig: "sig_prov".to_string(),
            not_after: 0,
        },
        models: vec![ModelCard {
            name: "my_model".to_string(),
            backend: "ollama".to_string(),
            context_length: 2048,
            price_msat_per_mtok_in: 100000,
            price_msat_per_mtok_out: 200000,
        }],
        payout: Payout {
            rail: "bolt12".to_string(),
            address: "addr_prov".to_string(),
        },
    };

    prov_ws.send(Message::Text(serde_json::to_string(&register_frame).unwrap().into())).await.unwrap();

    let msg = prov_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Registered { provider } => {
            assert_eq!(provider, "provider_a");
        }
        other => panic!("Expected Registered frame, got {:?}", other),
    }

    // 3. Connect the consumer client
    let consumer_url = format!("ws://127.0.0.1:{}/ws?token=ahp_consumer_a", port);
    let (mut cons_ws, _) = connect_async(consumer_url).await.unwrap();

    let session_id = "session_123".to_string();
    let open_frame = Frame::Open {
        session_id: session_id.clone(),
        envelope: Envelope {
            provider: "provider_a".to_string(),
            consumer: "consumer_a".to_string(),
            model: "my_model".to_string(),
            max_tokens: 100,
            est_input_tokens: 50,
            payment: Payment::Balance { token: "dummy_pay".to_string() },
            consumer_keybind: Keybind {
                x25519_pub: "pub_key_cons".to_string(),
                sig: "sig_cons".to_string(),
                not_after: 0,
            },
        },
    };

    cons_ws.send(Message::Text(serde_json::to_string(&open_frame).unwrap().into())).await.unwrap();

    // Verify OpenOk on consumer side
    let msg = cons_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::OpenOk { session_id: s_id, .. } => {
            assert_eq!(s_id, session_id);
        }
        other => panic!("Expected OpenOk, got {:?}", other),
    }

    // Verify Deliver(Open) on provider side
    let msg = prov_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Deliver { session_id: s_id, frame } => {
            assert_eq!(s_id, session_id);
            match *frame {
                Frame::Open { session_id: inside_s_id, envelope } => {
                    assert_eq!(inside_s_id, session_id);
                    assert_eq!(envelope.provider, "provider_a");
                    assert_eq!(envelope.consumer, "consumer_a");
                    assert_eq!(envelope.model, "my_model");
                }
                other => panic!("Expected Deliver(Open), got Deliver({:?})", other),
            }
        }
        other => panic!("Expected Deliver frame, got {:?}", other),
    }

    // 4. Verify Req from consumer is delivered to provider
    let req_frame = Frame::Req {
        session_id: session_id.clone(),
        blob: "opaque_req_blob".to_string(),
    };
    cons_ws.send(Message::Text(serde_json::to_string(&req_frame).unwrap().into())).await.unwrap();

    let msg = prov_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Deliver { session_id: s_id, frame } => {
            assert_eq!(s_id, session_id);
            match *frame {
                Frame::Req { session_id: inside_s_id, blob } => {
                    assert_eq!(inside_s_id, session_id);
                    assert_eq!(blob, "opaque_req_blob");
                }
                other => panic!("Expected Deliver(Req), got Deliver({:?})", other),
            }
        }
        other => panic!("Expected Deliver frame for Req, got {:?}", other),
    }

    // 5. Verify Res from provider is delivered to consumer
    let res_frame = Frame::Res {
        session_id: session_id.clone(),
        blob: "opaque_res_blob".to_string(),
    };
    prov_ws.send(Message::Text(serde_json::to_string(&res_frame).unwrap().into())).await.unwrap();

    let msg = cons_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Deliver { session_id: s_id, frame } => {
            assert_eq!(s_id, session_id);
            match *frame {
                Frame::Res { session_id: inside_s_id, blob } => {
                    assert_eq!(inside_s_id, session_id);
                    assert_eq!(blob, "opaque_res_blob");
                }
                other => panic!("Expected Deliver(Res), got Deliver({:?})", other),
            }
        }
        other => panic!("Expected Deliver frame for Res, got {:?}", other),
    }

    // 6. Connect third unrelated client and verify injection prevention
    let unrelated_url = format!("ws://127.0.0.1:{}/ws?token=ahp_unrelated_x", port);
    let (mut unrel_ws, _) = connect_async(unrelated_url).await.unwrap();

    let malicious_frame = Frame::Req {
        session_id: session_id.clone(),
        blob: "malicious_injection".to_string(),
    };
    unrel_ws.send(Message::Text(serde_json::to_string(&malicious_frame).unwrap().into())).await.unwrap();

    // The unrelated client should receive an Error frame
    let msg = unrel_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Error { code, .. } => {
            assert_eq!(code, ErrorCode::AuthFailed);
        }
        other => panic!("Expected Error frame on unrelated client, got {:?}", other),
    }

    // Verify neither consumer nor provider received any forwarded frames
    let check_no_msg_cons = tokio::time::timeout(tokio::time::Duration::from_millis(50), cons_ws.next()).await;
    assert!(check_no_msg_cons.is_err());

    let check_no_msg_prov = tokio::time::timeout(tokio::time::Duration::from_millis(50), prov_ws.next()).await;
    assert!(check_no_msg_prov.is_err());
}

#[tokio::test]
async fn test_non_dev_mode_consumer_verification() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    struct MockAuthenticator;
    impl charon_gateway::Authenticator for MockAuthenticator {
        fn authenticate<'a>(
            &'a self,
            token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ErrorCode>> + Send + 'a>> {
            Box::pin(async move {
                match token {
                    "consumer_token" => Ok("consumer_a".to_string()),
                    "provider_token" => Ok("provider_a".to_string()),
                    _ => Err(ErrorCode::AuthFailed),
                }
            })
        }
    }

    let authenticator = Arc::new(MockAuthenticator);
    let payment_verifier = Arc::new(DevPaymentVerifier);
    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        false, // disable_auth = false (non-dev mode)
        1000,
        21000,
    ));

    let state_clone = state.clone();
    tokio::spawn(async move {
        run_server(state_clone, listener).await.unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // 1. Connect and register provider
    let provider_url = format!("ws://127.0.0.1:{}/ws?token=provider_token", port);
    let (mut prov_ws, _) = connect_async(provider_url).await.unwrap();

    let register_frame = Frame::Register {
        ahp_token: "provider_token".to_string(),
        keybind: Keybind {
            x25519_pub: "pub_key_prov".to_string(),
            sig: "sig_prov".to_string(),
            not_after: 0,
        },
        models: vec![ModelCard {
            name: "my_model".to_string(),
            backend: "ollama".to_string(),
            context_length: 2048,
            price_msat_per_mtok_in: 100000,
            price_msat_per_mtok_out: 200000,
        }],
        payout: Payout {
            rail: "bolt12".to_string(),
            address: "addr_prov".to_string(),
        },
    };
    prov_ws.send(Message::Text(serde_json::to_string(&register_frame).unwrap().into())).await.unwrap();

    let msg = prov_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Registered { provider } => {
            assert_eq!(provider, "provider_a");
        }
        other => panic!("Expected Registered, got {:?}", other),
    }

    // 2. Connect consumer
    let consumer_url = format!("ws://127.0.0.1:{}/ws?token=consumer_token", port);
    let (mut cons_ws, _) = connect_async(consumer_url).await.unwrap();

    // 3. Try to open with mismatched consumer principal
    let session_id = "session_456".to_string();
    let open_mismatch = Frame::Open {
        session_id: session_id.clone(),
        envelope: Envelope {
            provider: "provider_a".to_string(),
            consumer: "wrong_consumer_principal".to_string(),
            model: "my_model".to_string(),
            max_tokens: 100,
            est_input_tokens: 50,
            payment: Payment::Balance { token: "dummy_pay".to_string() },
            consumer_keybind: Keybind {
                x25519_pub: "pub_key_cons".to_string(),
                sig: "sig_cons".to_string(),
                not_after: 0,
            },
        },
    };
    cons_ws.send(Message::Text(serde_json::to_string(&open_mismatch).unwrap().into())).await.unwrap();

    // Expect Error frame with AuthFailed on consumer side
    let msg = cons_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Error { code, .. } => {
            assert_eq!(code, ErrorCode::AuthFailed);
        }
        other => panic!("Expected Error, got {:?}", other),
    }

    // 4. Try to open with correct consumer principal
    let open_correct = Frame::Open {
        session_id: session_id.clone(),
        envelope: Envelope {
            provider: "provider_a".to_string(),
            consumer: "consumer_a".to_string(),
            model: "my_model".to_string(),
            max_tokens: 100,
            est_input_tokens: 50,
            payment: Payment::Balance { token: "dummy_pay".to_string() },
            consumer_keybind: Keybind {
                x25519_pub: "pub_key_cons".to_string(),
                sig: "sig_cons".to_string(),
                not_after: 0,
            },
        },
    };
    cons_ws.send(Message::Text(serde_json::to_string(&open_correct).unwrap().into())).await.unwrap();

    // Expect OpenOk
    let msg = cons_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::OpenOk { session_id: s_id, .. } => {
            assert_eq!(s_id, session_id);
        }
        other => panic!("Expected OpenOk, got {:?}", other),
    }

    // Verify Deliver(Open) on provider side retains envelope.consumer == "consumer_a"
    let msg = prov_ws.next().await.unwrap().unwrap();
    let text = msg.to_text().unwrap();
    let resp: Frame = serde_json::from_str(text).unwrap();
    match resp {
        Frame::Deliver { session_id: s_id, frame } => {
            assert_eq!(s_id, session_id);
            match *frame {
                Frame::Open { session_id: inside_s_id, envelope } => {
                    assert_eq!(inside_s_id, session_id);
                    assert_eq!(envelope.provider, "provider_a");
                    assert_eq!(envelope.consumer, "consumer_a");
                    assert_eq!(envelope.model, "my_model");
                }
                other => panic!("Expected Deliver(Open), got Deliver({:?})", other),
            }
        }
        other => panic!("Expected Deliver frame, got {:?}", other),
    }
}

#[tokio::test]
async fn test_cors_preflight_and_origins() {
    // Set custom CORS origins env var for testing
    std::env::set_var("CHARON_CORS_ORIGINS", "https://test-dashboard.charon.nuts.services,http://localhost:12345");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    let authenticator = Arc::new(GnosisAuthenticator::new("".to_string(), true));
    let payment_verifier = Arc::new(DevPaymentVerifier);
    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        true, // disable_auth
        1000,
        21000,
    ));

    let state_clone = state.clone();
    tokio::spawn(async move {
        run_server(state_clone, listener).await.unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // 1. Send OPTIONS preflight request to /v1/directory from an allowed origin
    let preflight_url = format!("http://127.0.0.1:{}/v1/directory", port);
    let res = client.request(reqwest::Method::OPTIONS, &preflight_url)
        .header("Origin", "https://test-dashboard.charon.nuts.services")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "authorization, content-type")
        .send()
        .await
        .unwrap();

    assert!(res.status().is_success());

    // Verify CORS response headers
    let cors_origin = res.headers().get("access-control-allow-origin").unwrap().to_str().unwrap();
    assert_eq!(cors_origin, "https://test-dashboard.charon.nuts.services");

    let cors_methods = res.headers().get("access-control-allow-methods").unwrap().to_str().unwrap();
    assert!(cors_methods.contains("GET"));
    assert!(cors_methods.contains("POST"));
    assert!(cors_methods.contains("OPTIONS"));

    let cors_headers = res.headers().get("access-control-allow-headers").unwrap().to_str().unwrap();
    assert!(cors_headers.to_lowercase().contains("authorization"));
    assert!(cors_headers.to_lowercase().contains("content-type"));

    // 2. Try OPTIONS request from a disallowed origin
    let res_disallowed = client.request(reqwest::Method::OPTIONS, &preflight_url)
        .header("Origin", "https://evil.com")
        .header("Access-Control-Request-Method", "GET")
        .send()
        .await
        .unwrap();

    assert!(res_disallowed.headers().get("access-control-allow-origin").is_none());

    // Clean up env
    std::env::remove_var("CHARON_CORS_ORIGINS");
}

#[tokio::test]
async fn test_wallet_history_retention_and_isolation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    struct MockAuthenticator;
    impl charon_gateway::Authenticator for MockAuthenticator {
        fn authenticate<'a>(
            &'a self,
            token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, ErrorCode>> + Send + 'a>> {
            Box::pin(async move {
                match token {
                    "token_a" => Ok("consumer_a".to_string()),
                    "token_b" => Ok("consumer_b".to_string()),
                    _ => Err(ErrorCode::AuthFailed),
                }
            })
        }
    }

    let authenticator = Arc::new(MockAuthenticator);
    let payment_verifier = Arc::new(DevPaymentVerifier);
    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        false, // disable_auth = false (so it checks Bearer token)
        1000,
        21000,
    ));

    // Populate wallet histories
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let old_ts = now.saturating_sub(15 * 24 * 60 * 60); // 15 days ago (older than 14 days)
    let new_ts = now.saturating_sub(5 * 24 * 60 * 60);  // 5 days ago (newer than 14 days)

    {
        let mut wallets = state.wallets.lock().unwrap();
        
        // consumer_a: has one new entry and one old entry
        let wallet_a = wallets.entry("consumer_a".to_string()).or_default();
        wallet_a.balance_msat = 10_000_000_000;
        wallet_a.history.push(charon_gateway::WalletEntry {
            ts: old_ts,
            r#type: "settlement".to_string(),
            amount_msat: -5000,
            status: "settled".to_string(),
        });
        wallet_a.history.push(charon_gateway::WalletEntry {
            ts: new_ts,
            r#type: "settlement".to_string(),
            amount_msat: -2000,
            status: "settled".to_string(),
        });

        // consumer_b: has one entry at current timestamp
        let wallet_b = wallets.entry("consumer_b".to_string()).or_default();
        wallet_b.balance_msat = 5_000_000_000;
        wallet_b.history.push(charon_gateway::WalletEntry {
            ts: now,
            r#type: "settlement".to_string(),
            amount_msat: -1000,
            status: "settled".to_string(),
        });
    }

    let state_clone = state.clone();
    tokio::spawn(async move {
        run_server(state_clone, listener).await.unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // 1. Check isolation: consumer_a should not see consumer_b's entries, and old entries are excluded
    let url_history = format!("http://127.0.0.1:{}/v1/wallet/history", port);
    
    let res_a = client.get(&url_history)
        .header("Authorization", "Bearer token_a")
        .send()
        .await
        .unwrap();
    assert_eq!(res_a.status(), reqwest::StatusCode::OK);
    
    let body_a: serde_json::Value = res_a.json().await.unwrap();
    let entries_a = body_a.get("entries").unwrap().as_array().unwrap();
    
    // Check retention: only the entry older than 14 days is excluded (1 remaining)
    assert_eq!(entries_a.len(), 1);
    let entry_a = &entries_a[0];
    assert_eq!(entry_a.get("ts").unwrap().as_u64().unwrap(), new_ts);
    assert_eq!(entry_a.get("amount_msat").unwrap().as_i64().unwrap(), -2000);
    assert_eq!(entry_a.get("type").unwrap().as_str().unwrap(), "settlement");

    // 2. Check consumer_b's history isolation
    let res_b = client.get(&url_history)
        .header("Authorization", "Bearer token_b")
        .send()
        .await
        .unwrap();
    assert_eq!(res_b.status(), reqwest::StatusCode::OK);
    
    let body_b: serde_json::Value = res_b.json().await.unwrap();
    let entries_b = body_b.get("entries").unwrap().as_array().unwrap();
    
    // consumer_b has exactly 1 entry
    assert_eq!(entries_b.len(), 1);
    let entry_b = &entries_b[0];
    assert_eq!(entry_b.get("ts").unwrap().as_u64().unwrap(), now);
    assert_eq!(entry_b.get("amount_msat").unwrap().as_i64().unwrap(), -1000);

    // 3. Verify balance isolation
    let url_balance = format!("http://127.0.0.1:{}/v1/wallet/balance", port);
    
    let bal_res_a = client.get(&url_balance)
        .header("Authorization", "Bearer token_a")
        .send()
        .await
        .unwrap();
    let bal_a: serde_json::Value = bal_res_a.json().await.unwrap();
    assert_eq!(bal_a.get("balance_msat").unwrap().as_u64().unwrap(), 10_000_000_000);

    let bal_res_b = client.get(&url_balance)
        .header("Authorization", "Bearer token_b")
        .send()
        .await
        .unwrap();
    let bal_b: serde_json::Value = bal_res_b.json().await.unwrap();
    assert_eq!(bal_b.get("balance_msat").unwrap().as_u64().unwrap(), 5_000_000_000);
}



