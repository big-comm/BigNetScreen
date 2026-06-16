//! Ponto de entrada do aplicativo gráfico BigNetScreen.

mod app;

use app::AppModel;
use relm4::RelmApp;

const APP_ID: &str = "br.com.biglinux.BigNetScreen";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("BigNetScreen {} iniciando", env!("CARGO_PKG_VERSION"));

    let app = RelmApp::new(APP_ID);
    app.run::<AppModel>(());
}
