//! Manual visual regression harness. Run in an isolated GTK session.
use super::*;
use relm4::{
    adw::{self, prelude::*},
    gtk,
    prelude::*,
};

#[test]
#[ignore = "requires GTK, BIGNETSCREEN_LAYOUT_DIR and a screenshot client"]
fn layout_pages_and_breakpoints() {
    let dir = std::path::PathBuf::from(
        std::env::var("BIGNETSCREEN_LAYOUT_DIR").expect("snapshot directory"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    super::preview::tests::fixture(&dir.join("sample.webm"), true, false);
    super::preview::tests::fixture(&dir.join("sample.ogg"), false, true);
    adw::init().unwrap();
    let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = errors.clone();
    let css = gtk::CssProvider::new();
    css.connect_parsing_error(move |_, _, error| observed.borrow_mut().push(error.to_string()));
    css.load_from_string(include_str!("../style.css"));
    assert!(errors.borrow().is_empty(), "CSS: {:?}", errors.borrow());
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let searches = std::rc::Rc::new(std::cell::Cell::new(0));
    let observed_searches = searches.clone();
    let home = home::HomePage::builder()
        .launch(())
        .connect_receiver(move |_, output| {
            if matches!(output, home::HomeOutput::Rescan) {
                observed_searches.set(observed_searches.get() + 1);
            }
        });
    let devices = devices::DevicesPage::builder().launch(()).detach();
    let media = media::MediaPage::builder().launch(()).detach();
    let settings = settings::SettingsPage::builder().launch(()).detach();
    let entries: Vec<_> = [
        ("Living room", SinkKind::Chromecast),
        ("Projector", SinkKind::WfdP2p),
        ("AirPlay receiver", SinkKind::AirPlay),
        ("Bedroom display", SinkKind::Chromecast),
        ("Meeting room", SinkKind::WfdP2p),
        ("Family TV", SinkKind::WfdP2p),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (name, kind))| DeviceEntry {
        id: i.to_string(),
        name: name.into(),
        protocol: protocol_label(kind),
        address: format!("192.168.1.{}", i + 20),
        kind,
        castable: kind.is_castable(),
        state: SinkState::Disconnected,
        detail: String::new(),
        mode: String::new(),
    })
    .collect();
    home.emit(home::HomeMsg::Devices(entries.clone()));
    home.emit(home::HomeMsg::VirtualAvailable(true));
    devices.emit(devices::DevicesMsg::Devices(entries.clone()));
    media.emit(media::MediaMsg::Targets(vec![(
        "1".into(),
        "Projector".into(),
    )]));
    let window = adw::Window::builder()
        .default_width(1050)
        .default_height(860)
        .build();
    let context = gtk::glib::MainContext::default();
    let flush = |ms| {
        context.block_on(gtk::glib::timeout_future(std::time::Duration::from_millis(
            ms,
        )))
    };
    let capture = |name: &str| {
        flush(500);
        std::fs::write(dir.join("ready"), name).unwrap();
        for _ in 0..150 {
            if dir.join(format!("{name}.png")).exists() {
                return;
            }
            flush(100);
        }
        panic!("screenshot client timed out for {name}");
    };
    home.widgets().find_devices.emit_clicked();
    flush(100);
    assert_eq!(
        searches.get(),
        1,
        "search button must request discovery once"
    );
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    window.set_content(Some(home.widget()));
    window.present();
    capture("home-dark");
    home.emit(home::HomeMsg::Selected("1".into()));
    capture("actions-dark");
    window.set_content(Some(devices.widget()));
    capture("devices-dark");
    devices.emit(devices::DevicesMsg::SetFilter(devices::Filter::Miracast));
    devices.emit(devices::DevicesMsg::Devices(entries[..1].to_vec()));
    capture("devices-filtered-dark");
    devices.emit(devices::DevicesMsg::Devices(Vec::new()));
    devices.emit(devices::DevicesMsg::Discovery(Discovery::Unreachable));
    capture("devices-unreachable-dark");
    devices.emit(devices::DevicesMsg::SetFilter(devices::Filter::All));
    devices.emit(devices::DevicesMsg::Discovery(Discovery::Finished));
    devices.emit(devices::DevicesMsg::Devices(entries.clone()));
    home.emit(home::HomeMsg::Back);
    home.emit(home::HomeMsg::Session(Some(SessionInfo {
        id: "web".into(),
        name: "Web browsers".into(),
        kind: None,
        protocol: "Web".into(),
        address: String::new(),
        mode: describe_mode(1920, 1080, 30, 2),
        state: SinkState::Streaming,
        measurable: false,
        probe_failed: false,
        round_trip_ms: None,
        access: Some(nd_core::sink::SinkAccess {
            url: "http://192.168.1.20:8080".into(),
            pin: "4821".into(),
        }),
    })));
    window.set_content(Some(home.widget()));
    capture("home-streaming-dark");
    home.emit(home::HomeMsg::Session(None));
    window.set_content(Some(settings.widget()));
    capture("settings-dark");
    window.set_content(Some(media.widget()));
    flush(1200);
    media.widgets().kind_group.set_active(1);
    flush(1200);
    media.emit(media::MediaMsg::Picked(vec![dir.join("sample.webm")]));
    flush(1000);
    capture("videos-dark");
    media.emit(media::MediaMsg::Clear);
    media.widgets().kind_group.set_active(2);
    flush(1200);
    media.emit(media::MediaMsg::Picked(vec![dir.join("sample.ogg")]));
    flush(1000);
    capture("music-dark");
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
    capture("music-light");
    window.set_default_size(480, 700);
    capture("music-compact");
    window.set_content(Some(home.widget()));
    capture("actions-compact");
    window.set_content(Some(devices.widget()));
    capture("devices-compact");
    window.set_content(Some(settings.widget()));
    capture("settings-compact");
    window.close();
}
