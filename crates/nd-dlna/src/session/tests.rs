use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// A 320x240 VP8 and Vorbis film of `seconds`, written for this test.
fn test_film(name: &str, seconds: u32) -> std::path::PathBuf {
    pipeline::init().unwrap();
    let path = std::env::temp_dir().join(format!("bns-dlna-{name}-{}.webm", std::process::id()));
    let fixture = gst::parse::launch(&format!(
        "webmmux name=mux ! filesink location=\"{}\" \
         videotestsrc num-buffers={} ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! vp8enc deadline=1 ! mux. \
         audiotestsrc num-buffers={} ! audio/x-raw,rate=48000 ! vorbisenc ! mux.",
        path.display(),
        seconds * 30,
        // audiotestsrc sends 1024 samples per buffer.
        seconds * 48_000 / 1024 + 1,
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let guard = PipelineGuard::new(fixture.clone());
    fixture.set_state(gst::State::Playing).unwrap();
    let message = fixture
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .unwrap();
    assert_eq!(message.type_(), gst::MessageType::Eos, "{message:?}");
    drop(guard);
    path
}

/// One HTTP request from a client of the renderer's control URL: its
/// headers and body.
async fn read_request(socket: &mut BufReader<TcpStream>) -> (String, String) {
    let mut headers = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(socket.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().unwrap();
        }
        headers.push_str(&line);
    }
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    (headers, String::from_utf8(body).unwrap())
}

/// Opens the stream at `url` and returns it past the response headers.
async fn fetch(url: &str) -> BufReader<TcpStream> {
    let endpoint = Endpoint::parse(url).unwrap();
    let mut client = TcpStream::connect(endpoint.addr).await.unwrap();
    client
        .write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
                endpoint.path, endpoint.authority
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut client = BufReader::new(client);
    loop {
        let mut line = String::new();
        assert!(client.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break client;
        }
    }
}

#[tokio::test]
async fn abandoned_start_is_handed_over_again() {
    let path = test_film("restart", 40);
    let source = CaptureSource::media_file(
        nd_core::capture::MediaPlayback {
            control: None,
            start: None,
            source: nd_core::media::MediaSource::File(path.clone()),
            kind: nd_core::media::MediaKind::Video,
            title: "DLNA restart test".into(),
        },
        (426, 240),
    );
    let previous = nd_core::settings::current();
    nd_core::settings::set_in_memory(&nd_core::settings::Settings {
        port: 0,
        system_audio: false,
        microphone: false,
        hardware_encoding: false,
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = Endpoint::parse(&format!(
        "http://{}/control",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let status = SinkStatus::new();
    let (_cancel, cancelled) = tokio::sync::watch::channel(false);
    let uris = std::sync::atomic::AtomicU32::new(0);
    let renderer = async {
        let mut watching = None;
        let mut held_since = None;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let (headers, body) = read_request(&mut socket).await;
            let mut reply = String::new();
            if headers.contains("#SetAVTransportURI") {
                let handed = uris.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let mut client = fetch(crate::upnp::tag_text(&body, "CurrentURI").unwrap()).await;
                let mut packet = [0; 188];
                client.read_exact(&mut packet).await.unwrap();
                assert_eq!(packet[0], 0x47);
                if handed == 2 {
                    // This time the picture starts: the connection is held
                    // and read, as by a television playing it.
                    held_since = Some(tokio::time::Instant::now());
                    watching = Some(tokio::spawn(async move {
                        let mut buffer = vec![0; 65536];
                        while matches!(client.read(&mut buffer).await, Ok(n) if n > 0) {}
                    }));
                }
                // The first time it is dropped at once: the start failed.
            } else if headers.contains("#GetTransportInfo") {
                // Still "playing" the stream it gave up on, as the
                // Panasonic reports for half a minute; stopped from the
                // remote a few seconds into the second hand-over.
                let playing =
                    held_since.is_none_or(|since| since.elapsed() < Duration::from_secs(4));
                let state = if playing { "PLAYING" } else { "STOPPED" };
                reply = format!(
                    "<s:Envelope><s:Body><u:GetTransportInfoResponse>\
                     <CurrentTransportState>{state}</CurrentTransportState>\
                     </u:GetTransportInfoResponse></s:Body></s:Envelope>"
                );
            }
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            if headers.contains("#Stop")
                && held_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(4))
            {
                break;
            }
        }
        if let Some(watching) = watching {
            watching.abort();
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(40), async {
        tokio::join!(
            stream(control.addr.ip(), &control, &source, &status, cancelled),
            renderer
        )
    })
    .await;
    nd_core::settings::set_in_memory(&previous);
    std::fs::remove_file(path).unwrap();
    result
        .expect("the session must end once the renderer stops")
        .0
        .unwrap();
    assert_eq!(
        uris.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "handed over once more after the failed start, and not after the real stop"
    );
}

#[tokio::test]
async fn remote_pause_holds_the_file() {
    use std::sync::atomic::{AtomicBool, Ordering};
    pipeline::init().unwrap();
    let file = gst::parse::launch("videotestsrc is-live=true ! fakesink")
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let _guard = PipelineGuard::new(file.clone());
    file.set_state(gst::State::Playing).unwrap();
    let target = |pipeline: &gst::Pipeline| match pipeline.state(gst::ClockTime::ZERO) {
        (_, current, gst::State::VoidPending) => current,
        (_, _, pending) => pending,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = Endpoint::parse(&format!(
        "http://{}/control",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let paused_here = AtomicBool::new(false);
    let renderer = async {
        // What the renderer answers each poll, and the file's state the
        // previous answer must have left.
        for (answer, expected) in [
            ("PLAYING", gst::State::Playing),
            ("PAUSED_PLAYBACK", gst::State::Playing),
            ("PAUSED_PLAYBACK", gst::State::Paused),
            ("PLAYING", gst::State::Paused),
            ("STOPPED", gst::State::Playing),
            ("STOPPED", gst::State::Playing),
        ] {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(target(&file), expected, "before answering {answer}");
            assert_eq!(
                paused_here.load(Ordering::SeqCst),
                expected == gst::State::Paused
            );
            let reply = format!(
                "<s:Envelope><s:Body><u:GetTransportInfoResponse>\
                 <CurrentTransportState>{answer}</CurrentTransportState>\
                 </u:GetTransportInfoResponse></s:Body></s:Envelope>"
            );
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            until_renderer_stops(
                &control,
                &paused_here,
                &file,
                true,
                Duration::from_millis(50)
            ),
            renderer
        )
    })
    .await
    .expect("two idle answers after playing must end the session");
}

#[tokio::test]
async fn delayed_get_reports_actual_pixels() {
    let path = test_film("delayed", 12);
    let playback = nd_core::media::FilePlaybackControl::default();
    let source = CaptureSource::media_file(
        nd_core::capture::MediaPlayback {
            control: Some(playback.clone()),
            start: Some(nd_core::media::PlaybackStart {
                seconds: 1.0,
                paused: false,
                volume: 0.25,
                muted: true,
                height: 0,
            }),
            source: nd_core::media::MediaSource::File(path.clone()),
            kind: nd_core::media::MediaKind::Video,
            title: "DLNA test".into(),
        },
        // The 16:9 frame `tv_frame` gives this 4:3 film at its own size.
        (426, 240),
    );
    let previous = nd_core::settings::current();
    nd_core::settings::set_in_memory(&nd_core::settings::Settings {
        port: 0,
        system_audio: false,
        microphone: false,
        hardware_encoding: false,
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = Endpoint::parse(&format!(
        "http://{}/control",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let status = SinkStatus::new();
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let renderer = async {
        let mut media = None;
        let mut stops = 0;
        let mut plays = 0;
        let mut uris = 0;
        let mut paused = false;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let body = String::from_utf8(body).unwrap();
            if headers.contains("#SetAVTransportURI") {
                uris += 1;
                assert!(!body.contains("resolution="));
                // Fetch inside SetAVTransportURI, more than two seconds
                // in: the caps must still be measured once it starts.
                tokio::time::sleep(Duration::from_millis(2100)).await;
                let url = crate::upnp::tag_text(&body, "CurrentURI").unwrap();
                let endpoint = Endpoint::parse(url).unwrap();
                let mut client = TcpStream::connect(endpoint.addr).await.unwrap();
                client
                    .write_all(
                        format!(
                            "GET {} HTTP/1.1\r\nHost: {}\r\n\r\n",
                            endpoint.path, endpoint.authority
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let mut client = BufReader::new(client);
                loop {
                    let mut line = String::new();
                    assert!(client.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut packet = [0; 188];
                client.read_exact(&mut packet).await.unwrap();
                assert_eq!(packet[0], 0x47, "receiver must get MPEG-TS");
                media = Some(client);
            }
            if headers.contains("#Play") {
                plays += 1;
            }
            // The resume is refused once, as by a renderer that gave up
            // on the silent stream during the pause.
            let refused = plays == 2;
            socket
                .write_all(if refused {
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                })
                .await
                .unwrap();
            if headers.contains("#Pause") {
                paused = true;
                playback
                    .command(&nd_core::media::MediaCommand::SetPaused(false))
                    .unwrap();
            }
            if plays == 3 && headers.contains("#Play") {
                // The refused resume handed the stream over again.
                assert!(paused, "Play again only after a Pause");
                assert_eq!(uris, 2);
                cancel.send_replace(true);
            }
            if headers.contains("#Play") && plays == 1 {
                let link = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if let Some(link) = status.link() {
                            break link;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("caps must be reported after the late GET");
                assert_eq!(
                    (link.width, link.height),
                    (426, 240),
                    "a file must reach the television in the frame it was given"
                );
                assert_eq!(status.state(), SinkState::Streaming);
                use nd_core::media::MediaCommand;
                assert!(playback.state().can_pause);
                assert!((playback.state().volume.unwrap() - 0.25).abs() < 0.000_001);
                assert_eq!(playback.state().muted, Some(true));
                assert!(playback.state().seconds >= 0.9);
                playback.command(&MediaCommand::SetVolume(0.37)).unwrap();
                playback.command(&MediaCommand::SetMute(false)).unwrap();
                assert!((playback.state().volume.unwrap() - 0.37).abs() < 0.000_001);
                assert_eq!(playback.state().muted, Some(false));
                // A paused file pipeline must reach the renderer as Pause.
                playback.command(&MediaCommand::SetPaused(true)).unwrap();
            }
            if headers.contains("#Stop") {
                stops += 1;
                if stops == 2 {
                    break;
                }
            }
        }
        drop(media);
    };
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(
            stream(control.addr.ip(), &control, &source, &status, cancelled),
            renderer
        )
    })
    .await;
    nd_core::settings::set_in_memory(&previous);
    std::fs::remove_file(path).unwrap();
    result.expect("DLNA handshake must not deadlock").0.unwrap();
}
