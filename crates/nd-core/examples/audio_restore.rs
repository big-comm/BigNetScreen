//! Read-only audio diagnostics; never modifies host defaults.

#[tokio::main]
async fn main() {
    println!("{:?}", nd_core::audio_state::Defaults::now().await);
}
