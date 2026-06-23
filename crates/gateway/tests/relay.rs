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

