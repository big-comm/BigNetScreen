//! Local TLS regression; this does not emulate a physical Cast firmware.
use super::*;
use rustls::pki_types::PrivateKeyDer;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

async fn receive<R: AsyncRead + Unpin>(reader: &mut R) -> CastMessage {
    tokio::time::timeout(Duration::from_secs(5), async {
        let length = reader.read_u32().await.unwrap() as usize;
        assert!(length <= MAX_MESSAGE_BYTES);
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await.unwrap();
        CastMessage::decode(&bytes[..]).unwrap()
    })
    .await
    .expect("local TLS peer did not receive the expected control message")
}

async fn reply<W: AsyncWrite + Unpin>(writer: &mut W, source: &str, namespace: &str, value: Value) {
    let message = CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: source.into(),
        destination_id: SOURCE_ID.into(),
        namespace: namespace.into(),
        payload_type: PayloadType::Str as i32,
        payload_utf8: Some(value.to_string()),
        payload_binary: None,
    };
    let bytes = message.encode_to_vec();
    writer.write_u32(bytes.len() as u32).await.unwrap();
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}

fn tls_acceptor() -> TlsAcceptor {
    let certificate =
        CertificateDer::from(include_bytes!("../tests/fixtures/test-receiver.der").to_vec());
    let root = CertificateDer::from(include_bytes!("../tests/fixtures/test-ca.der").to_vec());
    let key =
        PrivateKeyDer::try_from(include_bytes!("../tests/fixtures/test-receiver-key.der").to_vec())
            .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate, root], key)
    .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

#[tokio::test]
async fn twenty_tls_sessions_keep_heartbeat_until_scoped_stop_then_close() {
    let acceptor = tls_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server =
        tokio::spawn(async move {
            for cycle in 0..20 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let session_id = format!("our-session-{cycle}");
                let transport_id = format!("our-transport-{cycle}");
                let mut stop_request = None;
                let mut confirmed = false;
                let mut app_closed = false;
                loop {
                    let message = receive(&mut stream).await;
                    let payload: Value =
                        serde_json::from_str(message.payload_utf8.as_deref().unwrap()).unwrap();
                    match payload["type"].as_str().unwrap() {
                        "CONNECT" => assert!(!app_closed),
                        "PING" => {
                            reply(
                                &mut stream,
                                PLATFORM_DEST,
                                NS_HEARTBEAT,
                                json!({"type":"PONG"}),
                            )
                            .await
                        }
                        "LAUNCH" => {
                            assert_eq!(message.namespace, NS_RECEIVER);
                            reply(
                                &mut stream,
                                PLATFORM_DEST,
                                NS_RECEIVER,
                                json!({
                                    "type":"RECEIVER_STATUS", "requestId":payload["requestId"],
                                    "status":{"applications":[{"appId":DEFAULT_MEDIA_RECEIVER,
                                        "sessionId":session_id,"transportId":transport_id}]}
                                }),
                            )
                            .await;
                        }
                        "STOP" => {
                            assert_eq!(message.destination_id, PLATFORM_DEST);
                            assert_eq!(payload["sessionId"], session_id);
                            assert!(stop_request.is_none());
                            stop_request = Some(payload["requestId"].clone());
                            // Deliberately withhold STOP confirmation until PONG:
                            // disabling heartbeat before STOP would deadlock here.
                            reply(
                                &mut stream,
                                PLATFORM_DEST,
                                NS_HEARTBEAT,
                                json!({"type":"PING"}),
                            )
                            .await;
                        }
                        "PONG" => {
                            assert_eq!(message.namespace, NS_HEARTBEAT);
                            let request_id = stop_request.take().expect("unexpected PONG");
                            reply(&mut stream, PLATFORM_DEST, NS_RECEIVER, json!({
                            "type":"RECEIVER_STATUS", "requestId":request_id,
                            "status":{"applications":[{"sessionId":"another-senders-session"}]}
                        })).await;
                            confirmed = true;
                        }
                        "CLOSE" => {
                            assert!(confirmed, "connection closed before STOP was acknowledged");
                            if message.destination_id == transport_id {
                                assert!(!app_closed);
                                app_closed = true;
                            } else {
                                assert_eq!(message.destination_id, PLATFORM_DEST);
                                assert!(app_closed, "platform CLOSE preceded app CLOSE");
                                break;
                            }
                        }
                        kind => panic!("unexpected control message: {kind}"),
                    }
                }
                stream.shutdown().await.unwrap();
            }
        });
    let client = async {
        for _ in 0..20 {
            let channel = CastChannel::connect_to(address.ip(), address.port())
                .await
                .unwrap();
            let application = channel.launch(DEFAULT_MEDIA_RECEIVER).await.unwrap();
            channel.finish_app(&application, Ok(())).await.unwrap();
        }
        server.await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(30), client)
        .await
        .unwrap();
}

#[tokio::test]
async fn media_load_applies_position_and_mute_before_play_and_refuses_unconfirmed_volume() {
    let acceptor = tls_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cases = [(false, true), (true, true), (false, false)];
    let server = tokio::spawn(async move {
        for (paused, confirmed_volume) in cases {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let mut commands = Vec::new();
            let mut stopped = false;
            loop {
                let message = receive(&mut stream).await;
                let payload: Value =
                    serde_json::from_str(message.payload_utf8.as_deref().unwrap()).unwrap();
                let kind = payload["type"].as_str().unwrap();
                match kind {
                    "CONNECT" => {}
                    "PING" => reply(&mut stream, PLATFORM_DEST, NS_HEARTBEAT, json!({"type":"PONG"})).await,
                    "LAUNCH" => reply(&mut stream, PLATFORM_DEST, NS_RECEIVER, json!({
                        "type":"RECEIVER_STATUS", "requestId":payload["requestId"],
                        "status":{"applications":[{"appId":DEFAULT_MEDIA_RECEIVER, "sessionId":"owned", "transportId":"media-transport"}]}
                    })).await,
                    "LOAD" | "SET_VOLUME" | "PLAY" => {
                        assert_eq!(message.namespace, NS_MEDIA);
                        assert_eq!(message.destination_id, "media-transport");
                        match kind {
                            "LOAD" => {
                                assert!(commands.is_empty());
                                assert_eq!(payload["autoplay"], false);
                                assert_eq!(payload["currentTime"], 12.0);
                                assert_eq!(payload["media"]["contentId"], "https://example.invalid/movie");
                            }
                            "SET_VOLUME" => {
                                assert_eq!(commands, ["LOAD"]);
                                assert_eq!(payload["mediaSessionId"], 7);
                                assert_eq!(payload["volume"], json!({"level":0.25, "muted":true}));
                            }
                            "PLAY" => {
                                assert!(!paused && confirmed_volume);
                                assert_eq!(commands, ["LOAD", "SET_VOLUME"]);
                                assert_eq!(payload["mediaSessionId"], 7);
                            }
                            _ => unreachable!(),
                        }
                        commands.push(kind.to_string());
                        let volume = if kind != "LOAD" && confirmed_volume { json!({"level":0.25,"muted":true}) } else { json!({"level":1.0,"muted":false}) };
                        reply(&mut stream, "media-transport", NS_MEDIA, json!({
                            "type":"MEDIA_STATUS", "requestId":payload["requestId"], "status":[{
                                "mediaSessionId":7,"currentTime":12.0,"volume":volume,
                                "playerState":if kind == "PLAY" {"PLAYING"} else {"PAUSED"},
                                "supportedMediaCommands":15,
                                "media":{"contentId":"https://example.invalid/movie","duration":60.0}
                            }]
                        })).await;
                    }
                    "STOP" => {
                        assert_eq!(payload["sessionId"], "owned");
                        stopped = true;
                        reply(&mut stream, PLATFORM_DEST, NS_RECEIVER, json!({"type":"RECEIVER_STATUS", "requestId":payload["requestId"], "status":{"applications":[]}})).await;
                    }
                    "CLOSE" => {
                        assert!(stopped);
                        if message.destination_id == PLATFORM_DEST {
                            assert_eq!(commands.len(), if !paused && confirmed_volume { 3 } else { 2 });
                            break;
                        }
                    }
                    other => panic!("unexpected {other}"),
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        for (paused, confirmed_volume) in cases {
            let channel = CastChannel::connect_to(address.ip(), address.port())
                .await
                .unwrap();
            let app = channel.launch(DEFAULT_MEDIA_RECEIVER).await.unwrap();
            let item = crate::media::MediaItem::url(
                "https://example.invalid/movie".into(),
                "video/mp4".into(),
                "Movie".into(),
                false,
            )
            .unwrap();
            let result = channel
                .load_item(
                    &app,
                    "https://example.invalid/movie",
                    &item,
                    "test",
                    Some(nd_core::media::PlaybackStart {
                        seconds: 12.0,
                        paused,
                        volume: 0.25,
                        muted: true,
                    }),
                )
                .await;
            assert_eq!(result.is_ok(), confirmed_volume, "{result:?}");
            channel.finish_app(&app, Ok(())).await.unwrap();
        }
        server.await.unwrap();
    })
    .await
    .unwrap();
}
