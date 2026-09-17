//! Read-only audio diagnostics. Capture never changes default devices.
//!
//! Session-manager changes cannot be distinguished reliably from user choices.
//! Do not rewrite defaults after capture teardown.

use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub sink: Option<String>,
    pub source: Option<String>,
}

impl Defaults {
    pub async fn now() -> Self {
        let (sink, source) = tokio::join!(
            read_default("get-default-sink"),
            read_default("get-default-source")
        );
        Self { sink, source }
    }
    pub fn is_empty(&self) -> bool {
        self.sink.is_none() && self.source.is_none()
    }
}

async fn read_default(command: &str) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("pactl")
            .arg(command)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!name.is_empty() && !name.starts_with('@')).then_some(name)
}
