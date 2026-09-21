//! Entry point of the BigNetScreen graphical application.

mod app;
mod i18n;
mod ndi_setup;
mod pages;
mod shutdown;

use app::AppModel;
use relm4::RelmApp;

const APP_ID: &str = "br.com.biglinux.BigNetScreen";
static STARTED: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// The build date, in the `yy.mm.dd` form the package names its releases with.
///
/// Stamped by `build.rs`. Not Cargo's version: that one is crate metadata and
/// says nothing about which rolling build someone is running.
pub const APP_VERSION: &str = env!("BIGNETSCREEN_VERSION");

fn main() {
    // These diagnostics must work without a display, portal or media plugins.
    if std::env::args_os().len() == 2 {
        match std::env::args_os()
            .nth(1)
            .as_deref()
            .and_then(std::ffi::OsStr::to_str)
        {
            Some("--version") | Some("-V") => {
                println!("BigNetScreen {APP_VERSION}");
                return;
            }
            Some("--help") | Some("-h") => {
                println!("BigNetScreen — share a Linux screen with network receivers.\n\nUsage: bignetscreen [--version | --help]\n\nWithout arguments, opens the graphical application.");
                return;
            }
            _ => {}
        }
    }
    std::sync::LazyLock::force(&STARTED);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    i18n::init();

    tracing::info!(
        version = APP_VERSION,
        sandboxed = nd_capture::is_sandboxed(),
        "BigNetScreen starting"
    );

    // Useful in a bug report: which encoder will actually be used.
    let driver = nd_net::detect_gpu_driver();
    match nd_core::pipeline::best_encoder(driver) {
        Ok(encoder) => tracing::info!(?driver, ?encoder, "video encoding"),
        Err(err) => tracing::error!(?driver, %err, "no H.264 encoder available"),
    }

    // Read once at start-up so that the first session already obeys them, and
    // so a broken settings file is reported here rather than at the moment
    // someone hits cast.
    let settings = nd_core::settings::current();
    tracing::info!(?settings, "preferences");

    let app = RelmApp::new(APP_ID);
    relm4::set_global_css(include_str!("style.css"));
    app.run::<AppModel>(());
}
