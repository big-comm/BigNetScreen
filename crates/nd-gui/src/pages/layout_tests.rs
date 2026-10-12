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
    let layer = gtk::Overlay::new();
    let options = options::OptionsPanel::builder()
        .launch(layer.clone())
        .detach();
    let share = share::SharePage::builder().launch(()).detach();
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
        blocked: None,
    })
    .collect();
    share.emit(share::ShareMsg::Devices(entries.clone()));
    options.emit(options::OptionsMsg::VirtualAvailable(false));
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
    let split = adw::OverlaySplitView::builder()
        .sidebar(options.widget())
        .content(share.widget())
        .min_sidebar_width(300.0)
        .build();
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    layer.set_child(Some(&split));
    window.set_content(Some(&layer));
    window.present();
    capture("share-dark");
    share.emit(share::ShareMsg::Devices(Vec::new()));
    share.emit(share::ShareMsg::Discovery(Discovery::Unreachable));
    capture("share-unreachable-dark");
    share.emit(share::ShareMsg::Discovery(Discovery::Finished));
    share.emit(share::ShareMsg::Devices(entries.clone()));
    share.emit(share::ShareMsg::Session(Some(SessionInfo {
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
    capture("share-streaming-dark");
    share.emit(share::ShareMsg::Session(None));
    options.emit(options::OptionsMsg::Card(
        hover_cards::CardEvent::Activated(3),
    ));
    capture("quality-card-dark");
    options.emit(options::OptionsMsg::Card(hover_cards::CardEvent::Escape));
    share.widgets().files_slot.append(media.widget());
    share.emit(share::ShareMsg::Files {
        files: true,
        chosen: false,
    });
    flush(1200);
    media.widgets().kind_group.set_active(1);
    flush(1200);
    capture("files-dark");
    media.emit(media::MediaMsg::Picked(vec![dir.join("sample.webm")]));
    share.emit(share::ShareMsg::Files {
        files: true,
        chosen: true,
    });
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
    split.set_collapsed(true);
    window.set_default_size(480, 700);
    capture("music-compact");
    share.emit(share::ShareMsg::Files {
        files: false,
        chosen: false,
    });
    capture("share-compact");
    window.set_content(Some(settings.widget()));
    capture("settings-compact");
    window.close();
}
