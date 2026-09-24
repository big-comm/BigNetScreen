//! Running one cast, from opening the capture to the session ending.

use std::sync::Arc;

use tokio::sync::watch;

use nd_core::capture::SourceType;
use nd_core::sink::Sink;

/// How a cast ended.
pub struct Outcome {
    pub error: Option<String>,
    /// NDI was asked for and its runtime is not installed. Separate from
    /// `error` because it is the one failure a client can offer to repair.
    pub ndi_runtime_unavailable: bool,
}

impl Outcome {
    fn ok() -> Self {
        Self {
            error: None,
            ndi_runtime_unavailable: false,
        }
    }

    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ndi_runtime_unavailable: false,
        }
    }
}

pub async fn run(
    sink: Arc<dyn Sink>,
    id: String,
    source_type: SourceType,
    mut cancel: watch::Receiver<bool>,
) -> Outcome {
    if id == nd_ndi::ID {
        match ndi_ready(&mut cancel).await {
            Ok(true) => {}
            Ok(false) => return Outcome::ok(),
            Err(outcome) => return outcome,
        }
    }
    if *cancel.borrow() {
        return Outcome::ok();
    }

    // The card is normally already there, put up when the switch was turned
    // on. This covers the switch having been set in a previous run of the
    // service, or the sound server having been restarted since.
    //
    // **Failing here ends the cast rather than falling back.** Asking to send
    // only what was routed to the card and being given the whole computer's
    // sound instead is the one outcome worse than not transmitting: it is a
    // notification, or a private call, leaving the machine. Better to say why
    // now than to be quietly wrong.
    let settings = nd_core::settings::current();
    if settings.system_audio && settings.virtual_audio {
        if let Err(err) = nd_core::virtual_sink::ensure().await {
            return Outcome::failed(format!(
                "the virtual sound card could not be created, and sending the \
                 computer's whole output instead is not what was asked for: {err}"
            ));
        }
    }

    let backend = tokio::select! {
        backend = nd_capture::select_backend_for(source_type, sink.max_source_size()) => backend,
        _ = cancel.changed() => return Outcome::ok(),
    };
    let result = async {
        let source = tokio::select! {
            result = backend.start(source_type) => result.map_err(|e| e.to_string())?,
            _ = cancel.changed() => return Ok(()),
        };
        stream_until_cancelled(&sink, source, &mut cancel).await
    }
    .await;
    let _ = backend.stop().await;
    match result {
        Ok(()) => Outcome::ok(),
        Err(error) => Outcome::failed(error),
    }
}

/// `Ok(true)` to go ahead, `Ok(false)` if it was cancelled meanwhile.
async fn ndi_ready(cancel: &mut watch::Receiver<bool>) -> Result<bool, Outcome> {
    let check = tokio::select! {
        result = tokio::task::spawn_blocking(nd_ndi::runtime_check) => result.ok(),
        _ = cancel.changed() => return Ok(false),
    };
    if *cancel.borrow() {
        return Ok(false);
    }
    match check {
        Some(nd_ndi::RuntimeCheck::Ready { .. }) => Ok(true),
        // Installing it again would change nothing, so this is a plain failure
        // rather than the offer to install.
        Some(nd_ndi::RuntimeCheck::CpuUnsupported { .. }) => {
            Err(Outcome::failed("ndi-cpu-unsupported".into()))
        }
        Some(nd_ndi::RuntimeCheck::Missing) | None => Err(Outcome {
            error: None,
            ndi_runtime_unavailable: true,
        }),
    }
}

async fn stream_until_cancelled(
    sink: &Arc<dyn Sink>,
    source: nd_core::capture::CaptureSource,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    if *cancel.borrow() {
        return Ok(());
    }
    let playing = sink.start_stream(source);
    tokio::pin!(playing);
    tokio::select! {
        biased;
        result = &mut playing => return result.map_err(|e| e.to_string()),
        _ = cancel.changed() => {}
    }
    // Not discarded: this is the receiver confirming it let the session go.
    // When it fails the device is still holding the old session and the next
    // connection will be refused, which is worth saying at the time rather
    // than leaving somebody to discover it two attempts later.
    if let Err(err) = sink.stop_stream().await {
        tracing::warn!(%err, "the receiver did not confirm the session ended");
    }
    playing.await.map_err(|e| e.to_string())
}
