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

async fn reply<W: AsyncWrite + Unpin>(writer: &mut W, namespace: &str, value: Value) {
    let message = CastMessage {
        protocol_version: ProtocolVersion::Castv210 as i32,
        source_id: PLATFORM_DEST.into(),
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

#[tokio::test]
async fn twenty_tls_sessions_keep_heartbeat_until_scoped_stop_then_close() {
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
    let acceptor = TlsAcceptor::from(Arc::new(config));
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
                        "PING" => reply(&mut stream, NS_HEARTBEAT, json!({"type":"PONG"})).await,
                        "LAUNCH" => {
                            assert_eq!(message.namespace, NS_RECEIVER);
                            reply(
                                &mut stream,
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
                            reply(&mut stream, NS_HEARTBEAT, json!({"type":"PING"})).await;
                        }
                        "PONG" => {
                            assert_eq!(message.namespace, NS_HEARTBEAT);
                            let request_id = stop_request.take().expect("unexpected PONG");
                            reply(&mut stream, NS_RECEIVER, json!({
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
