//! The window: a sidebar, a page for each thing the application does, and the
//! state that outlives any one of them.
//!
//! It used to own the session too — discovery, the running cast, the files
//! being sent — and closing the window ended all of it. That half now lives in
//! `nd_service` and answers on D-Bus, so this is a **client**: it asks, draws
//! what comes back, and leaving stops the drawing and nothing else.
//!
//! Design points worth knowing:
//!
//! - **the service reports shapes, this writes the sentences**. It may be
//!   started by D-Bus activation with a bare environment and its other clients
//!   may be under another locale, so nothing translated crosses the bus. Every
//!   string a person reads is produced here, by [`describe_status`] and the
//!   page modules;
//! - **failures are visible**: a protocol that will not start becomes a banner
//!   saying why, not a `warn` in a log with the window stuck on "Searching…";
//! - **the pages are told, they do not ask**: each update pushes the current
//!   list to whichever pages show one, so two parts of the window cannot
//!   disagree about what is connected;
//! - all text goes through `tr!()` (gettext).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_chromecast::file_server::MediaFile;
use nd_chromecast::media::MediaStatus;
use nd_core::capture::SourceType;
use nd_core::media::PlaybackState;
use nd_core::settings::{self, Settings};
use nd_service::dbus::ServiceProxy;
use nd_service::wire;

use crate::pages::devices::{DevicesMsg, DevicesOutput, DevicesPage};
use crate::pages::home::{HomeMsg, HomeOutput, HomePage};
use crate::pages::media::{MediaMsg, MediaOutput, MediaPage};
use crate::pages::settings::{SettingsMsg, SettingsOutput, SettingsPage};
use crate::pages::{DeviceEntry, Page, SessionInfo};
use crate::{tr, tr_n};

/// What the service last told us. Everything drawn comes from here.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ServiceState {
    receivers: Vec<wire::Receiver>,
    session: wire::Session,
    link_probe: String,
    status: wire::Status,
    issues: Vec<wire::Issue>,
    media: wire::Media,
    virtual_available: bool,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            receivers: Vec::new(),
            session: wire::Session::idle(),
            link_probe: "pending".into(),
            status: wire::Status::of("searching"),
            issues: Vec::new(),
            media: wire::Media::idle(),
            virtual_available: false,
        }
    }
}

pub struct AppModel {
    home: Controller<HomePage>,
    devices: Controller<DevicesPage>,
    media: Controller<MediaPage>,
    settings_page: Controller<SettingsPage>,

    page: Page,
    /// The session service, once it has answered. `None` while connecting, or
    /// after it could not be reached — which is a banner, not a silent window.
    service: Option<ServiceProxy<'static>>,
    state: ServiceState,
    /// The sentence under the window title, in the person's language.
    status: String,
    failure: Option<String>,
    settings_changed: bool,
    /// Issues the person has dismissed, so the banner does not come back for
    /// something they have already read.
    dismissed: Vec<wire::Issue>,
    /// What will be captured when a receiver is picked.
    source_type: SourceType,
    settings: Settings,
    ndi_installing: std::rc::Rc<std::cell::Cell<bool>>,
    ndi_install_dialog: Option<adw::AlertDialog>,
    allow_close: std::rc::Rc<std::cell::Cell<bool>>,
}

#[derive(Debug)]
pub enum AppMsg {
    CloseRequested,
    Navigate(Page),
    /// Start streaming to this receiver.
    Cast(String),
    PublishNdi(SourceType),
    /// Share to web browsers on the network.
    PublishWeb(SourceType),
    InstallNdi,
    /// Start streaming this to this receiver, in one step.
    CastWith(String, SourceType),
    Stop,
    Rescan,
    DismissIssues,
    ShowIssues,
    ShowFailure,
    SetAutoDiscovery(bool),
    SettingsChanged(Settings),
    SettingsSaved(Result<(), String>),
    /// Send these files to the receiver with this id.
    SendMedia(Vec<MediaFile>, String),
    CancelMedia,
    ControlMedia(nd_core::media::MediaCommand),
    /// Send files to this receiver (from the devices page).
    MediaTarget(String),
    About,
}

/// A proxy that does not print its entire property cache.
///
/// `relm4` logs every command output at `info`, and `ServiceProxy`'s own
/// `Debug` is the whole cache — one connection filled a bug report with the
/// value of every property it had just read.
pub struct Connection(ServiceProxy<'static>);

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connection")
    }
}

#[derive(Debug)]
pub enum AppCmd {
    NdiInstalled(std::result::Result<(), String>),
    NdiInstallOffer(bool),
    /// The service answered and is ours to talk to.
    Connected(Box<Connection>),
    /// The service could not be reached, with the reason to show.
    Unreachable(String),
    /// A fresh reading of everything the service publishes.
    State(Box<ServiceState>),
    /// A call the service refused.
    Refused(String),
    Done,
    PreferencesFlushed(Result<(), String>),
}

#[relm4::component(pub)]
impl Component for AppModel {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = AppCmd;

    view! {
        adw::ApplicationWindow {
            set_title: Some("BigNetScreen"),
            add_css_class: "bns-window",
            set_default_width: 1280,
            set_default_height: 860,
            set_width_request: 480,
            set_height_request: 480,

            #[name = "split"]
            adw::OverlaySplitView {
                set_max_sidebar_width: 270.0,
                set_min_sidebar_width: 250.0,
                set_collapsed: false,
                set_show_sidebar: true,

                #[wrap(Some)]
                // No header bar over the sidebar: an empty one only pushed the
                // name of the application a title bar's height down the page.
                // The identity block sits at the very top instead, wrapped in a
                // `WindowHandle` so that area still drags the window — which is
                // what the header bar was quietly providing.
                set_sidebar = &gtk::Box {
                    add_css_class: "bns-sidebar",
                    set_orientation: gtk::Orientation::Vertical,

                    gtk::WindowHandle {
                        // The identity block.
                        gtk::Box {
                            set_spacing: 10,
                            add_css_class: "app-identity",

                            gtk::Image {
                                set_icon_name: Some("tv-symbolic"),
                                set_pixel_size: 22,
                                add_css_class: "app-mark",
                            },
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_valign: gtk::Align::Center,

                                gtk::Label {
                                    set_label: "BigNetScreen",
                                    set_xalign: 0.0,
                                    add_css_class: "title",
                                },
                                gtk::Label {
                                    set_label: &tr!("Share your screen wirelessly"),
                                    set_xalign: 0.0,
                                    add_css_class: "subtitle",
                                },
                            },
                        },
                    },

                    #[name = "nav"]
                        gtk::ListBox {
                            set_margin_all: 8,
                            add_css_class: "navigation-sidebar",
                            connect_row_selected[sender] => move |_, row| {
                                if let Some(row) = row {
                                    sender.input(AppMsg::Navigate(
                                        Page::all()[row.index().max(0) as usize],
                                    ));
                                }
                            },
                        },

                        gtk::Box { set_vexpand: true },

                        // What is connected, at the foot of the sidebar.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            add_css_class: "connected-card",
                            #[watch]
                            set_visible: !model.state.session.is_idle(),

                            gtk::Box {
                                set_spacing: 6,

                                gtk::Label {
                                    set_label: &tr!("Connected to"),
                                    set_hexpand: true,
                                    set_xalign: 0.0,
                                    add_css_class: "caption",
                                },
                                gtk::Label {
                                    set_label: "●",
                                    add_css_class: "live-dot",
                                },
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.state.session.display_name,
                                set_xalign: 0.0,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "heading",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.state.session.address,
                                set_xalign: 0.0,
                                add_css_class: "dim-label",
                            },
                        },

                        gtk::Box {
                            set_spacing: 12,
                            add_css_class: "sidebar-footer",

                            gtk::Button {
                                set_icon_name: "help-about-symbolic",
                                set_tooltip_text: Some(&tr!("About BigNetScreen")),
                                connect_clicked => AppMsg::About,
                            },
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_valign: gtk::Align::Center,
                                gtk::Label { set_label: "BigNetScreen", set_xalign: 0.0, add_css_class: "dim-label" },
                                gtk::Label { set_label: crate::APP_VERSION, set_xalign: 0.0, add_css_class: "caption", add_css_class: "dim-label" },
                            },
                        },
                },

                #[wrap(Some)]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        add_css_class: "flat",

                        #[name = "navigation_button"]
                        pack_start = &gtk::Button {
                            set_icon_name: "sidebar-show-symbolic",
                            set_tooltip_text: Some(&tr!("Show navigation")),
                            set_visible: false,
                        },

                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle {
                            #[watch]
                            set_title: &model.page.title(),
                            #[watch]
                            set_subtitle: &model.status,
                        },

                        pack_end = &gtk::Button {
                            set_label: &tr!("Stop"),
                            add_css_class: "destructive-action",
                            #[watch]
                            set_visible: model.is_busy(),
                            connect_clicked => AppMsg::Stop,
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,

                        adw::Banner {
                            #[watch]
                            set_revealed: !model.issues_summary().is_empty(),
                            #[watch]
                            set_title: &model.issues_summary(),
                            set_button_label: Some(&tr!("Details")),
                            connect_button_clicked => AppMsg::ShowIssues,
                        },

                        adw::Banner {
                            #[watch]
                            set_revealed: model.failure.is_some() || model.state.status.kind == "error",
                            #[watch]
                            set_title: &describe_refusal(model.failure.as_deref().unwrap_or(&model.state.status.detail)),
                            set_button_label: Some(&tr!("Details")),
                            connect_button_clicked => AppMsg::ShowFailure,
                        },

                        #[name = "stack"]
                        gtk::Stack {
                            set_vexpand: true,
                            // The visible page is set from `update_with_view`
                            // rather than watched here: the pages are added to
                            // the stack after this view is built, so a watch
                            // would fire once against an empty stack.
                            set_transition_type: gtk::StackTransitionType::Crossfade,
                        },
                    },
                },
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let current = settings::current();

        let home = HomePage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                // The home page names both halves of the decision in one
                // message: who receives, and what they receive.
                HomeOutput::PublishNdi(source) => AppMsg::PublishNdi(source),
                HomeOutput::PublishWeb(source) => AppMsg::PublishWeb(source),
                HomeOutput::Cast { id, source } => AppMsg::CastWith(id, source),
                HomeOutput::SendMedia(id) => AppMsg::MediaTarget(id),
                HomeOutput::Stop => AppMsg::Stop,
                HomeOutput::Rescan => AppMsg::Rescan,
                HomeOutput::AudioSettings => AppMsg::Navigate(Page::Settings),
            });
        let devices = DevicesPage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                DevicesOutput::Cast(id) => AppMsg::Cast(id),
                DevicesOutput::Stop => AppMsg::Stop,
                DevicesOutput::Rescan => AppMsg::Rescan,
                DevicesOutput::AutoDiscovery(on) => AppMsg::SetAutoDiscovery(on),
                DevicesOutput::SendMedia(id) => AppMsg::MediaTarget(id),
            });
        let media = MediaPage::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                MediaOutput::Send(files, target) => AppMsg::SendMedia(files, target),
                MediaOutput::Cancel => AppMsg::CancelMedia,
                MediaOutput::Control(command) => AppMsg::ControlMedia(command),
            });
        let settings_page = SettingsPage::builder().launch(()).forward(
            sender.input_sender(),
            |output| match output {
                SettingsOutput::Changed(settings) => AppMsg::SettingsChanged(settings),
                SettingsOutput::Saved(result) => AppMsg::SettingsSaved(result),
            },
        );

        let model = AppModel {
            failure: None,
            settings_changed: false,
            home,
            devices,
            media,
            settings_page,
            page: Page::Home,
            service: None,
            state: ServiceState::default(),
            status: tr!("Connecting…"),
            dismissed: Vec::new(),
            source_type: SourceType::Monitor,
            settings: current.clone(),
            ndi_installing: Default::default(),
            ndi_install_dialog: None,
            allow_close: Default::default(),
        };

        let widgets = view_output!();
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 1000px").expect("valid breakpoint"),
        );
        compact.add_setter(&widgets.split, "collapsed", Some(&true.to_value()));
        compact.add_setter(&widgets.split, "show-sidebar", Some(&false.to_value()));
        compact.add_setter(
            &widgets.navigation_button,
            "visible",
            Some(&true.to_value()),
        );
        big_gtk_kit::breakpoint::add(&root, compact);
        let split = widgets.split.downgrade();
        widgets.navigation_button.connect_clicked(move |_| {
            if let Some(split) = split.upgrade() {
                split.set_show_sidebar(true);
            }
        });

        let ndi_installing = model.ndi_installing.clone();
        let allow_close = model.allow_close.clone();
        let close_input = sender.input_sender().clone();
        root.connect_close_request(move |_| {
            if allow_close.get() {
                return gtk::glib::Propagation::Proceed;
            }
            if !ndi_installing.get() {
                close_input.emit(AppMsg::CloseRequested);
            }
            gtk::glib::Propagation::Stop
        });

        let mapped = std::cell::Cell::new(false);
        root.connect_map(move |window| {
            if mapped.replace(true) {
                return;
            }
            tracing::info!(
                elapsed_ms = crate::STARTED.elapsed().as_millis() as u64,
                renderer = ?window.renderer().map(|renderer| renderer.type_().name()),
                "main window mapped"
            );
            if let Some(clock) = window.frame_clock() {
                let painted = std::cell::Cell::new(false);
                clock.connect_after_paint(move |_| {
                    if !painted.replace(true) {
                        tracing::info!(
                            elapsed_ms = crate::STARTED.elapsed().as_millis() as u64,
                            "first window frame rendered"
                        );
                    }
                });
            }
        });

        // The sidebar's entries, in the same order as `Page::all()` — the
        // selection handler maps a row index straight onto that array.
        for page in Page::all() {
            let row = adw::ActionRow::builder().title(page.title()).build();
            row.add_prefix(&gtk::Image::from_icon_name(page.icon()));
            widgets.nav.append(&row);
        }
        if let Some(row) = widgets.nav.row_at_index(0) {
            widgets.nav.select_row(Some(&row));
        }

        widgets
            .stack
            .add_named(model.home.widget(), Some(Page::Home.id()));
        widgets
            .stack
            .add_named(model.devices.widget(), Some(Page::Devices.id()));
        widgets
            .stack
            .add_named(model.media.widget(), Some(Page::Media.id()));
        widgets
            .stack
            .add_named(model.settings_page.widget(), Some(Page::Settings.id()));

        // The pages are added after the view is built, so the first pass of
        // `set_visible_child_name` ran against an empty stack. Setting it here
        // is what keeps the window from opening blank for a fraction of a
        // second (and GTK from warning that "home" does not exist).
        widgets.stack.set_visible_child_name(model.page.id());

        model.home.emit(HomeMsg::Searching(current.auto_discovery));
        model
            .home
            .emit(HomeMsg::AudioSummary(audio_summary(&current)));
        follow_the_service(&sender);

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            AppMsg::CloseRequested => {
                if !self.settings_changed {
                    self.allow_close.set(true);
                    root.close();
                    return;
                }
                let service = self.service.clone();
                sender.oneshot_command(async move {
                    let result = relm4::spawn_blocking(settings::persist)
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|result| result.map_err(|err| err.to_string()));
                    if result.is_ok()
                        && let Some(service) = service
                    {
                        // Closing before the debounce fires must still notify the service.
                        let _ = tokio::time::timeout(
                            Duration::from_secs(10),
                            service.reload_settings(),
                        )
                        .await;
                    }
                    AppCmd::PreferencesFlushed(result)
                });
            }
            AppMsg::Navigate(page) => {
                self.page = page;
                if widgets.split.is_collapsed() {
                    widgets.split.set_show_sidebar(false);
                }
            }
            AppMsg::Cast(id) => self.begin_cast(id, &sender),
            AppMsg::PublishNdi(source) => {
                if self.ndi_installing.get() {
                    return;
                } else if !nd_ndi::available() {
                    self.status = tr!("NDI unavailable. See the NDI setup guide.");
                } else {
                    self.source_type = source;
                    self.begin_cast(nd_ndi::ID.into(), &sender);
                }
            }
            AppMsg::PublishWeb(source) => {
                if !nd_webrtc::available() {
                    self.status = tr!(
                        "Sharing with a web browser needs GStreamer's WebRTC plugins, which \
                         this system does not have."
                    );
                } else {
                    self.source_type = source;
                    self.begin_cast(nd_webrtc::ID.into(), &sender);
                }
            }
            AppMsg::InstallNdi => {
                if self.ndi_installing.get() {
                    return;
                }
                self.ndi_installing.set(true);
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Installing NDI…")),
                    Some(&tr!(
                        "Authenticate when prompted. Downloading and building the package may take a few minutes. Keep the app open until installation finishes."
                    )),
                );
                dialog.set_can_close(false);
                let spinner = gtk::Spinner::new();
                spinner.start();
                dialog.set_extra_child(Some(&spinner));
                dialog.present(Some(root));
                self.ndi_install_dialog = Some(dialog);
                sender.oneshot_command(async {
                    AppCmd::NdiInstalled(crate::ndi_setup::install().await)
                });
            }
            AppMsg::CastWith(id, source) => {
                self.source_type = source;
                tracing::info!(?source, "capture source chosen");
                self.begin_cast(id, &sender);
            }
            AppMsg::Stop | AppMsg::CancelMedia => {
                self.failure = None;
                self.status = tr!("Stopping…");
                self.call(&sender, |service| async move { service.stop().await });
            }
            AppMsg::Rescan => {
                self.failure = None;
                self.dismissed.clear();
                self.status = tr!("Searching…");
                self.call(&sender, |service| async move { service.rescan().await });
            }
            AppMsg::DismissIssues => self.dismissed = self.state.issues.clone(),
            AppMsg::ShowIssues => {
                let details = self
                    .state
                    .issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.provider, issue.reason))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let dialog =
                    adw::AlertDialog::new(Some(&tr!("Connection details")), Some(&details));
                dialog.add_response("close", &tr!("Close"));
                dialog.add_response("dismiss", &tr!("Got it"));
                let input = sender.input_sender().clone();
                dialog.connect_response(Some("dismiss"), move |_, _| {
                    input.emit(AppMsg::DismissIssues)
                });
                dialog.present(Some(root));
            }
            AppMsg::ShowFailure => {
                let detail = self.failure.as_deref().unwrap_or(&self.state.status.detail);
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Sharing could not be completed")),
                    Some(detail),
                );
                dialog.add_response("close", &tr!("Close"));
                dialog.add_response("settings", &tr!("Settings"));
                let input = sender.input_sender().clone();
                dialog.connect_response(Some("settings"), move |_, _| {
                    input.emit(AppMsg::Navigate(Page::Settings))
                });
                dialog.present(Some(root));
            }
            AppMsg::SetAutoDiscovery(on) => {
                self.settings_page.emit(SettingsMsg::SetAutoDiscovery(on));
            }
            AppMsg::SettingsChanged(new) => {
                self.settings_changed = true;
                self.failure = None;
                self.home.emit(HomeMsg::AudioSummary(audio_summary(&new)));
                self.settings = new;
                self.devices
                    .emit(DevicesMsg::SyncAutoDiscovery(self.settings.auto_discovery));
            }
            AppMsg::SettingsSaved(result) => match result {
                Ok(()) => self.call(
                    &sender,
                    |service| async move { service.reload_settings().await },
                ),
                Err(reason) => {
                    self.failure = Some(format!("could not save settings: {reason}"));
                    self.status = tr!("Could not save settings");
                }
            },
            AppMsg::MediaTarget(id) => {
                // Chosen on the devices page: the media page opens with that
                // receiver already selected, because the person just said so.
                self.media.emit(MediaMsg::Choose(id));
                self.page = Page::Media;
            }
            AppMsg::SendMedia(files, target) => {
                let paths: Vec<String> = files
                    .iter()
                    .map(|file| file.path.display().to_string())
                    .collect();
                self.call_with_settings(&sender, move |service| async move {
                    service.send_media(&target, &paths).await
                });
            }
            AppMsg::ControlMedia(command) => {
                use nd_core::media::MediaCommand;
                let (name, value, path) = match command {
                    MediaCommand::TogglePause => ("toggle-pause", 0.0, PathBuf::new()),
                    MediaCommand::SetPaused(paused) => {
                        (if paused { "pause" } else { "play" }, 0.0, PathBuf::new())
                    }
                    MediaCommand::SeekTo(seconds) => ("seek-to", seconds, PathBuf::new()),
                    MediaCommand::SetVolume(level) => ("volume", level, PathBuf::new()),
                    MediaCommand::SetMute(muted) => {
                        (if muted { "mute" } else { "unmute" }, 0.0, PathBuf::new())
                    }
                    MediaCommand::Next => ("next", 0.0, PathBuf::new()),
                    MediaCommand::SeekRelative(seconds) => ("seek", seconds, PathBuf::new()),
                    MediaCommand::Remove(path) => ("remove", 0.0, path),
                };
                let path = path.display().to_string();
                self.call(&sender, move |service| async move {
                    service.control_media(name, value, &path).await
                });
            }
            AppMsg::About => show_about(root),
        }

        // The page the model says is current.
        widgets.stack.set_visible_child_name(self.page.id());

        // A page can also be reached without touching the sidebar — "Media" on
        // the home page, or "Send media" on a device. The highlight has to
        // follow, or it points at the page the person just left.
        let index = Page::all()
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0) as i32;
        if widgets.nav.selected_row().map(|row| row.index()) != Some(index)
            && let Some(row) = widgets.nav.row_at_index(index)
        {
            widgets.nav.select_row(Some(&row));
        }

        self.update_view(widgets, sender);
    }

    /// Handles a background result **and repaints**.
    ///
    /// The repaint is not optional. `update_cmd_with_view` replaces the default
    /// cycle, so whoever implements it owns the view update — without the call
    /// at the end, nothing arriving from a command reaches the window.
    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        // Named on arrival, before anything is done with it. relm4 only
        // prints its `update_cmd_with_view` span when an event happens inside
        // it, so a variant that logs nothing leaves no trace at all — which
        // had this read as "the state never arrives" when it always did.
        tracing::debug!(
            kind = match &message {
                AppCmd::NdiInstalled(_) => "ndi-installed",
                AppCmd::NdiInstallOffer(_) => "ndi-install-offer",
                AppCmd::Connected(_) => "connected",
                AppCmd::Unreachable(_) => "unreachable",
                AppCmd::State(_) => "state",
                AppCmd::Refused(_) => "refused",
                AppCmd::Done => "done",
                AppCmd::PreferencesFlushed(_) => "preferences-flushed",
            },
            "command output arrived"
        );
        match message {
            AppCmd::Done => return,
            AppCmd::PreferencesFlushed(result) => match result {
                Ok(()) => {
                    self.allow_close.set(true);
                    root.close();
                }
                Err(reason) => {
                    let dialog =
                        adw::AlertDialog::new(Some(&tr!("Could not save settings")), Some(&reason));
                    dialog.add_response("back", &tr!("Keep open"));
                    dialog.add_response("close", &tr!("Close without saving"));
                    dialog.set_default_response(Some("back"));
                    dialog.set_close_response("back");
                    let allow_close = self.allow_close.clone();
                    let window = root.downgrade();
                    dialog.connect_response(Some("close"), move |_, _| {
                        allow_close.set(true);
                        if let Some(window) = window.upgrade() {
                            window.close();
                        }
                    });
                    dialog.present(Some(root));
                }
            },
            AppCmd::NdiInstalled(result) => self.ndi_installed(result, &sender, root),
            AppCmd::NdiInstallOffer(can_install) => {
                if self.state.status.kind == "ndi-runtime-missing" {
                    self.offer_ndi_install(can_install, &sender, root);
                }
            }
            AppCmd::Connected(connection) => {
                self.failure = None;
                tracing::info!("connected to the session service");
                self.service = Some(connection.0);
            }
            AppCmd::Unreachable(reason) => {
                tracing::error!(%reason, "the session service could not be reached");
                self.service = None;
                self.state = ServiceState::default();
                self.home.emit(HomeMsg::Searching(false));
                self.home.emit(HomeMsg::VirtualAvailable(false));
                self.push_devices();
                self.push_media_status();
                self.status = tr!("The sharing service is not responding. Try again.");
            }
            AppCmd::Refused(reason) => {
                self.status = describe_refusal(&reason);
                self.failure = Some(reason);
            }
            AppCmd::State(state) => {
                let receivers_changed = state.receivers != self.state.receivers;
                let session_changed = state.session != self.state.session
                    || state.link_probe != self.state.link_probe;
                let media_changed = state.media != self.state.media;
                let searching_changed = state.status.kind != self.state.status.kind;
                let virtual_changed = state.virtual_available != self.state.virtual_available;
                let ndi_missing = state.status.kind == "ndi-runtime-missing"
                    && self.state.status.kind != "ndi-runtime-missing";
                let was_streaming = !self.state.session.is_idle();
                self.state = *state;
                self.status = describe_status(&self.state.status, &self.state.session);
                if virtual_changed {
                    self.home
                        .emit(HomeMsg::VirtualAvailable(self.state.virtual_available));
                }
                if searching_changed {
                    self.home
                        .emit(HomeMsg::Searching(self.state.status.kind == "searching"));
                }
                if receivers_changed || session_changed {
                    self.push_devices();
                }
                if media_changed {
                    self.push_media_status();
                }
                if ndi_missing {
                    sender.oneshot_command(async {
                        AppCmd::NdiInstallOffer(crate::ndi_setup::can_install().await)
                    });
                }
                if was_streaming && self.state.session.is_idle() {
                    tracing::info!("the session ended");
                }
            }
        }

        // Repaint. Everything above only changed the model.
        self.update_view(widgets, sender);
    }
}

impl AppModel {
    /// Is anything running that the Stop button should end?
    fn is_busy(&self) -> bool {
        !self.state.session.is_idle() || self.state.media.active
    }

    /// Calls the service, turning a refusal into something the window can say.
    ///
    /// Every action goes through here so that no call site has to remember
    /// that the service may not be there yet.
    fn call<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
    where
        F: FnOnce(ServiceProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send,
    {
        let Some(service) = self.service.clone() else {
            sender.oneshot_command(async { AppCmd::Unreachable("not connected".into()) });
            return;
        };
        sender.oneshot_command(async move {
            match call(service).await {
                Ok(()) => AppCmd::Done,
                Err(err) => AppCmd::Refused(err.to_string()),
            }
        });
    }

    fn call_with_settings<F, Fut>(&self, sender: &ComponentSender<Self>, call: F)
    where
        F: FnOnce(ServiceProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<()>> + Send,
    {
        let settings_changed = self.settings_changed;
        self.call(sender, move |service| async move {
            if settings_changed {
                relm4::spawn_blocking(settings::persist)
                    .await
                    .map_err(|err| zbus::Error::Failure(err.to_string()))?
                    .map_err(|err| {
                        zbus::Error::Failure(format!("could not save settings: {err}"))
                    })?;
            }
            service.reload_settings().await?;
            call(service).await
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        self.failure = None;
        if self.is_busy() {
            self.status = tr!("A stream is already running");
            return;
        }
        let source = nd_service::engine::source_name(self.source_type).to_string();
        // The page a browser is shown is for a stranger on the network, not for
        // a client, so the service cannot write it: it has no locale. This does.
        let page_text = web_page_text();
        self.status = tr!("Connecting…");
        self.call_with_settings(sender, move |service| async move {
            service.cast(&id, &source, &page_text).await
        });
    }

    /// The receivers, as the pages want them.
    fn entries(&self) -> Vec<DeviceEntry> {
        self.state
            .receivers
            .iter()
            .map(DeviceEntry::from_wire)
            .collect()
    }

    /// Pushes the current list to every page that shows receivers.
    fn push_devices(&mut self) {
        let entries = self.entries();
        self.home.emit(HomeMsg::Devices(entries.clone()));
        self.home.emit(HomeMsg::Session(self.session_info()));
        self.devices.emit(DevicesMsg::Devices(entries));
        self.devices.emit(DevicesMsg::Active(
            (!self.state.session.is_idle()).then(|| self.state.session.id.clone()),
        ));
        self.media.emit(MediaMsg::Targets(self.media_receivers()));
    }

    fn push_media_status(&mut self) {
        if !self.state.media.active {
            self.media.emit(MediaMsg::Status(None));
            return;
        }
        self.media
            .emit(MediaMsg::Status(Some(media_status(&self.state.media))));
    }

    /// What is streaming right now.
    fn session_info(&self) -> Option<SessionInfo> {
        let session = &self.state.session;
        if session.is_idle() {
            return None;
        }
        let kind = wire::kind_from_name(&session.kind);
        let round_trip = (self.state.link_probe == "available").then_some(session.round_trip_ms);
        Some(SessionInfo {
            id: session.id.clone(),
            name: session.display_name.clone(),
            protocol: kind
                .map(crate::pages::protocol_label)
                .unwrap_or_else(|| session.kind.clone()),
            address: session.address.clone(),
            // The mode the two ends **agreed on**. Empty until then: printing
            // the preference instead would show "1920 × 1080" for a link that
            // came out at 1280 × 720.
            mode: crate::pages::describe_mode(
                session.width,
                session.height,
                session.fps,
                session.receivers,
            ),
            state: wire::state_from_name(&session.state)
                .unwrap_or(nd_core::sink::SinkState::Disconnected),
            measurable: session.measurable,
            probe_failed: self.state.link_probe == "unreachable",
            round_trip_ms: round_trip,
            access: (!session.url.is_empty()).then(|| nd_core::sink::SinkAccess {
                url: session.url.clone(),
                pin: session.pin.clone(),
            }),
        })
    }

    /// The receivers that can be sent a file, for the person to choose from.
    ///
    /// There is deliberately **no** "best guess": casting to a device is
    /// visible to whoever is standing in front of it, so it takes an explicit
    /// choice, every time.
    fn media_receivers(&self) -> Vec<(String, String)> {
        self.state
            .receivers
            .iter()
            .filter(|receiver| nd_service::engine::can_receive_files(receiver))
            .map(|receiver| (receiver.id.clone(), receiver.display_name.clone()))
            .collect()
    }

    /// The banner text: what is wrong that the person has not already read.
    fn issues_summary(&self) -> String {
        self.state
            .issues
            .iter()
            .filter(|issue| !self.dismissed.contains(issue))
            .map(|issue| friendly_reason(&issue.provider, &issue.reason))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn ndi_installed(
        &mut self,
        result: std::result::Result<(), String>,
        _sender: &ComponentSender<Self>,
        root: &adw::ApplicationWindow,
    ) {
        self.ndi_installing.set(false);
        if let Some(dialog) = self.ndi_install_dialog.take() {
            dialog.force_close();
        }
        let dialog = match result {
            Ok(()) => adw::AlertDialog::new(
                Some(&tr!("NDI installed")),
                Some(&tr!("Restart BigNetScreen to use NDI.")),
            ),
            Err(error) => {
                tracing::warn!(%error, "NDI installation failed or was cancelled");
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("NDI installation did not finish")),
                    Some(&tr!(
                        "Installation failed or authentication was cancelled. You can try again or install the library manually."
                    )),
                );
                let details = gtk::TextView::builder()
                    .editable(false)
                    .cursor_visible(false)
                    .monospace(true)
                    .wrap_mode(gtk::WrapMode::WordChar)
                    .build();
                details.buffer().set_text(&error);
                let scroll = gtk::ScrolledWindow::builder()
                    .min_content_height(140)
                    .max_content_height(200)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&details)
                    .build();
                dialog.set_extra_child(Some(&scroll));
                dialog
            }
        };
        dialog.add_response("close", &tr!("Close"));
        dialog.set_close_response("close");
        dialog.present(Some(root));
    }

    fn offer_ndi_install(
        &self,
        can_install: bool,
        sender: &ComponentSender<Self>,
        root: &adw::ApplicationWindow,
    ) {
        let body = if can_install {
            tr!(
                "NDI needs a proprietary library that is not included with BigNetScreen. Install ndi-sdk from the AUR and the required build tools? Your system will ask for administrator authentication. Other sharing methods work without it."
            )
        } else {
            tr!(
                "NDI needs a proprietary library that is not included with BigNetScreen. Install the NDI 5 or 6 runtime for your distribution, then restart the app. Other sharing methods work without it."
            )
        };
        let dialog = adw::AlertDialog::new(Some(&tr!("NDI runtime required")), Some(&body));
        dialog.add_response("close", &tr!("Close"));
        dialog.set_close_response("close");
        if can_install {
            dialog.add_response("install", &tr!("Install"));
            dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
            let input = sender.input_sender().clone();
            dialog.connect_response(Some("install"), move |_, _| {
                input.emit(AppMsg::InstallNdi);
            });
        }
        dialog.set_extra_child(Some(&gtk::LinkButton::with_label(
            if can_install {
                crate::ndi_setup::AUR_URL
            } else {
                crate::ndi_setup::GUIDE_URL
            },
            &if can_install {
                tr!("View ndi-sdk in the AUR")
            } else {
                tr!("NDI setup")
            },
        )));
        dialog.present(Some(root));
    }
}

/// Subscribes before reading, then applies only the properties that changed.
fn follow_the_service(sender: &ComponentSender<AppModel>) {
    sender.command(|out, shutdown| {
        shutdown.register(async move {
            let mut retry = Duration::from_secs(2);
            loop {
                let result: Result<(), String> = async {
                    let connection = zbus::Connection::session().await.map_err(|e| e.to_string())?;
                    let service = ServiceProxy::builder(&connection)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build().await.map_err(|e| e.to_string())?;
                    let properties = zbus::fdo::PropertiesProxy::builder(&connection)
                        .destination(nd_service::BUS_NAME).map_err(|e| e.to_string())?
                        .path(nd_service::OBJECT_PATH).map_err(|e| e.to_string())?
                        .build().await.map_err(|e| e.to_string())?;
                    let mut changes = properties.receive_properties_changed().await.map_err(|e| e.to_string())?;
                    let bus = zbus::fdo::DBusProxy::new(&connection).await.map_err(|e| e.to_string())?;
                    let mut owners = bus.receive_name_owner_changed_with_args(&[(0, nd_service::BUS_NAME)])
                        .await.map_err(|e| e.to_string())?;
                    let mut state = read_state(&properties).await?;
                    retry = Duration::from_secs(2);
                    out.send(AppCmd::Connected(Box::new(Connection(service.clone())))).map_err(|_| "window closed")?;
                    out.send(AppCmd::State(Box::new(state.clone()))).map_err(|_| "window closed")?;
                    let mut heartbeat = tokio::time::interval(Duration::from_secs(60));
                    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            change = changes.next() => {
                                let change = change.ok_or("service subscription ended")?;
                                let args = change.args().map_err(|e| e.to_string())?;
                                if args.interface_name().as_str() != nd_service::dbus::INTERFACE { continue; }
                                if args.invalidated_properties().is_empty() {
                                    for (name, value) in args.changed_properties() {
                                        state.apply_property(name, value.try_to_owned().map_err(|e| e.to_string())?)?;
                                    }
                                } else {
                                    state = read_state(&properties).await?;
                                }
                                out.send(AppCmd::State(Box::new(state.clone()))).map_err(|_| "window closed")?;
                            }
                            owner = owners.next() => {
                                let owner = owner.ok_or("session bus subscription ended")?;
                                let args = owner.args().map_err(|e| e.to_string())?;
                                if args.new_owner().is_none() || args.old_owner().is_some() {
                                    return Err("sharing service disconnected".into());
                                }
                            }
                            _ = heartbeat.tick() => {
                                tokio::time::timeout(Duration::from_secs(5), service.keep_alive()).await
                                    .map_err(|_| "sharing service did not answer")?
                                    .map_err(|e| e.to_string())?;
                            }
                        }
                    }
                }.await;
                if out.send(AppCmd::Unreachable(result.unwrap_err())).is_err() { return; }
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(30));
            }
        }).drop_on_shutdown()
    });
}

impl ServiceState {
    fn apply_property(
        &mut self,
        name: &str,
        value: zbus::zvariant::OwnedValue,
    ) -> Result<(), String> {
        use zbus::zvariant::Error;
        match name {
            "Receivers" => self.receivers = value.try_into().map_err(|e: Error| e.to_string())?,
            "Session" => self.session = value.try_into().map_err(|e: Error| e.to_string())?,
            "LinkProbe" => self.link_probe = value.try_into().map_err(|e: Error| e.to_string())?,
            "Status" => self.status = value.try_into().map_err(|e: Error| e.to_string())?,
            "Issues" => self.issues = value.try_into().map_err(|e: Error| e.to_string())?,
            "Media" => self.media = value.try_into().map_err(|e: Error| e.to_string())?,
            "VirtualAvailable" => {
                self.virtual_available = value.try_into().map_err(|e: Error| e.to_string())?
            }
            _ => {}
        }
        Ok(())
    }
}

async fn read_state(properties: &zbus::fdo::PropertiesProxy<'_>) -> Result<ServiceState, String> {
    let values = tokio::time::timeout(
        Duration::from_secs(5),
        properties.get_all(
            nd_service::dbus::INTERFACE
                .try_into()
                .map_err(|e: zbus::names::Error| e.to_string())?,
        ),
    )
    .await
    .map_err(|_| "sharing service did not answer")?
    .map_err(|e| e.to_string())?;
    let mut state = ServiceState::default();
    for required in [
        "Receivers",
        "Session",
        "Status",
        "Issues",
        "Media",
        "VirtualAvailable",
    ] {
        if !values.contains_key(required) {
            return Err(format!("service is missing {required}"));
        }
    }
    for (name, value) in values {
        state.apply_property(&name, value)?;
    }
    Ok(state)
}

/// The service's shape, in the person's language.
///
/// This is the whole reason the service publishes a `kind` instead of a
/// sentence: it has no locale worth trusting, and an applet next to this window
/// may be under another one.
fn describe_status(status: &wire::Status, session: &wire::Session) -> String {
    match status.kind.as_str() {
        "searching" => tr!("Searching…"),
        "found" => tr_n!(
            "{} receiver found",
            "{} receivers found",
            status.count as usize
        )
        .replace("{}", &status.count.to_string()),
        "empty" => tr!("No receivers found"),
        "discovery-off" => tr!("Automatic discovery is off"),
        "connecting" => format!("{} · {}", status.display_name, tr!("Connecting…")),
        "stopping" => tr!("Stopping…"),
        "sending" => tr!("Sending files…"),
        "ndi-runtime-missing" => tr!("NDI runtime required"),
        "error" => describe_refusal(&status.detail),
        "streaming" => {
            if session.width > 0 {
                format!(
                    "{} · {} × {} · {} Hz",
                    status.display_name, session.width, session.height, session.fps
                )
            } else {
                format!("{} · {}", status.display_name, tr!("Connected"))
            }
        }
        _ => status.kind.clone(),
    }
}

/// A refusal from the service, in the person's language.
///
/// The service answers with a tag for the cases it knows and the underlying
/// message for the rest, so the common refusals read as sentences and nothing
/// is swallowed.
fn describe_refusal(reason: &str) -> String {
    if reason.is_empty() {
        return String::new();
    }
    if reason.contains("virtual sound card") {
        return tr!(
            "Could not prepare the BigNetScreen sound output. Check the audio options in Settings and try again."
        );
    }
    if reason.contains("could not save settings") {
        return tr!("Could not save settings");
    }
    let tail = reason.rsplit(": ").next().unwrap_or(reason);
    match tail {
        "unknown-receiver" => tr!("That device is no longer available"),
        "not-castable" => tr!("shows up in discovery only"),
        "ndi-unavailable" => tr!("NDI unavailable. See the NDI setup guide."),
        "webrtc-unavailable" => tr!(
            "Sharing with a web browser needs GStreamer's WebRTC plugins, which this system \
             does not have."
        ),
        "a stream is already running" => tr!("A stream is already running"),
        nd_chromecast::identity::IDENTITY_CHANGED => tr!(
            "This receiver did not prove it is the same device used before, so the screen was \
             not sent. Someone on the network may be impersonating it."
        ),
        _ => tr!("Sharing could not be completed. Open Details to see the reason."),
    }
}

fn audio_summary(settings: &Settings) -> String {
    let sound = if !settings.system_audio {
        tr!("Application sound off")
    } else if settings.virtual_audio {
        tr!("Sound routed to BigNetScreen")
    } else {
        tr!("All computer sound")
    };
    let microphone = if settings.microphone {
        tr!("Microphone on")
    } else {
        tr!("Microphone off")
    };
    format!("{sound} · {microphone}")
}

/// Turns a provider's technical error into something a person can act on.
///
/// The service passes the provider's own words through untranslated, because
/// it has no catalogue and no locale worth trusting. This is where they become
/// a sentence — and it is the only reason the banner is not raw English.
fn friendly_reason(provider: &str, reason: &str) -> String {
    if provider == "wfd-p2p" {
        if nd_capture::is_sandboxed() {
            return tr!(
                "Miracast does not work in the Flatpak build (it needs the system NetworkManager)"
            );
        }
        // Said before the card is blamed: a desktop wired by Ethernet has no
        // card to blame, and the old wording sent someone hunting for a
        // setting that could not exist.
        if reason.contains("no Wi-Fi adapter") {
            return tr!(
                "Miracast needs a Wi-Fi adapter, and this computer has none. Casting over \
                 the network still works."
            );
        }
        if reason.contains("Wi-Fi P2P") || reason.contains("Wi-Fi Direct") {
            return tr!("Miracast unavailable: this Wi-Fi card does not support Wi-Fi Direct");
        }
        return tr!(
            "Miracast is unavailable. Check that Wi-Fi is enabled, or share with a web browser."
        );
    }
    if matches!(provider, "mdns" | "dlna") {
        return tr!("Local network discovery is unavailable. Check your network connection.");
    }
    tr!("Device discovery is unavailable. Open Details to see the reason.")
}

/// What the web page says, in the application's language.
fn web_page_text() -> Vec<(String, String)> {
    [
        (
            "prompt",
            tr!("Enter the PIN shown on the computer that is sharing."),
        ),
        ("join", tr!("Watch")),
        (
            "wrong_pin",
            tr!("That PIN is not right. Check the computer's screen."),
        ),
        (
            "locked",
            tr!("Too many attempts. Wait half a minute and try again."),
        ),
        ("connecting", tr!("Connecting…")),
        (
            "failed",
            tr!("Could not connect. Make sure both devices are on the same network."),
        ),
        ("ended", tr!("The sharing has ended.")),
        (
            "fullscreen_hint",
            tr!("Tap or click the picture for full screen."),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// The media page still speaks `MediaStatus`; the service speaks [`wire::Media`].
fn media_status(media: &wire::Media) -> MediaStatus {
    MediaStatus {
        position: media.index as usize,
        total: media.total as usize,
        title: media.title.clone(),
        queue: media.queue.iter().map(PathBuf::from).collect(),
        playback: PlaybackState {
            paused: media.paused,
            seconds: media.position_ms as f64 / 1000.0,
            duration: (media.duration_ms > 0).then(|| media.duration_ms as f64 / 1000.0),
            can_pause: media.can_pause,
            can_seek: media.can_seek,
            ..Default::default()
        },
        finished: media.finished,
        error: (!media.detail.is_empty()).then(|| media.detail.clone()),
        control_error: (!media.control_detail.is_empty()).then(|| media.control_detail.clone()),
    }
}

fn show_about(root: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("BigNetScreen")
        .application_icon("br.com.biglinux.BigNetScreen")
        .version(crate::APP_VERSION)
        .developer_name("BigCommunity")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/big-comm/BigNetScreen")
        .comments(tr!("Share your screen over Miracast, Chromecast and NDI"))
        .build();
    about.present(Some(root));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires isolated GTK and XDG_CONFIG_HOME=/proc/bns-leak-test"]
    fn save_error_dialog_releases_its_window() {
        assert_eq!(
            std::env::var("XDG_CONFIG_HOME").unwrap(),
            "/proc/bns-leak-test"
        );
        adw::init().unwrap();
        struct Finalized(std::rc::Rc<std::cell::Cell<bool>>);
        impl Drop for Finalized {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let context = gtk::glib::MainContext::default();
        for force in [false, true].into_iter().cycle().take(10) {
            let finalized = std::rc::Rc::new(std::cell::Cell::new(false));
            let controller = AppModel::builder().launch(());
            // SAFETY: this test exclusively owns the key and observes its destructor.
            unsafe {
                controller
                    .widget()
                    .set_data("bns-save-finalization", Finalized(finalized.clone()));
            }
            controller.widget().present();
            controller.state().get_mut().model.settings_changed = true;
            controller.emit(AppMsg::CloseRequested);
            for _ in 0..20 {
                context.block_on(gtk::glib::timeout_future(Duration::from_millis(50)));
                if controller.widget().visible_dialog().is_some() {
                    break;
                }
            }
            let dialog = controller
                .widget()
                .visible_dialog()
                .unwrap()
                .downcast::<adw::AlertDialog>()
                .unwrap();
            if force {
                controller.widget().destroy();
            } else {
                dialog.set_close_response("close");
                dialog.close();
            }
            drop(dialog);
            drop(controller);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(500)));
            assert!(
                finalized.get(),
                "save-error window did not finalize (force={force})"
            );
        }
        println!("save-error window census: new=10, fin=10");
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn navigation_tree_finalizes_after_window_close() {
        adw::init().unwrap();
        let finalized = std::rc::Rc::new(std::cell::Cell::new(0));
        struct Finalized(std::rc::Rc<std::cell::Cell<usize>>);
        impl Drop for Finalized {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let context = gtk::glib::MainContext::default();
        let cycles = std::env::var("BIGNETSCREEN_LEAK_CYCLES")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(5);
        for cycle in 1..=cycles {
            let controller = AppModel::builder().launch(());
            // SAFETY: this test owns this qdata key and never reads its value.
            // Its destructor observes finalization, rather than disposal.
            unsafe {
                controller
                    .widgets()
                    .split
                    .set_data("bns-test-finalization", Finalized(finalized.clone()));
            }
            controller.widget().present();
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            controller.widgets().split.set_show_sidebar(false);
            controller.widgets().navigation_button.emit_clicked();
            assert!(controller.widgets().split.shows_sidebar());
            controller.widget().destroy();
            drop(controller);
            context.block_on(gtk::glib::timeout_future(Duration::from_millis(100)));
            assert_eq!(finalized.get(), cycle, "navigation panel did not finalize");
        }
        println!("navigation census: new={cycles}, fin={}", finalized.get());
    }

    fn status(kind: &str) -> wire::Status {
        wire::Status::of(kind)
    }

    #[test]
    fn a_shape_the_client_does_not_know_is_shown_rather_than_swallowed() {
        // A newer service could publish a kind this build has no sentence for.
        // Printing the tag beats an empty subtitle that says nothing is wrong.
        let unknown = describe_status(&status("something-new"), &wire::Session::idle());
        assert_eq!(unknown, "something-new");
    }

    #[test]
    fn the_negotiated_mode_is_shown_only_once_there_is_one() {
        let mut session = wire::Session::idle();
        session.id = "x".into();
        let mut streaming = status("streaming");
        streaming.display_name = "TV".into();
        assert!(!describe_status(&streaming, &session).contains('×'));

        session.width = 1280;
        session.height = 720;
        session.fps = 60;
        let described = describe_status(&streaming, &session);
        assert!(described.contains("1280 × 720"), "{described}");
    }

    #[test]
    fn a_refusal_reads_as_a_sentence_even_wrapped_in_a_bus_error() {
        // zbus prefixes the reason with the error name; the tag is at the end.
        let wrapped = "org.freedesktop.DBus.Error.Failed: unknown-receiver";
        assert_eq!(
            describe_refusal(wrapped),
            describe_refusal("unknown-receiver")
        );
        assert!(!describe_refusal(wrapped).is_empty());
        assert_eq!(
            describe_refusal("something odd"),
            tr!("Sharing could not be completed. Open Details to see the reason.")
        );
        assert!(describe_refusal("").is_empty());
    }

    #[test]
    fn media_arrives_at_the_page_with_everything_it_draws() {
        let media = wire::Media {
            active: true,
            title: "Song".into(),
            queue: vec!["/music/a.mp3".into(), "/music/b.mp3".into()],
            index: 1,
            total: 2,
            position_ms: 30_500,
            duration_ms: 240_000,
            paused: true,
            can_pause: true,
            can_seek: false,
            finished: false,
            detail: String::new(),
            control_detail: "seek refused".into(),
        };
        let status = media_status(&media);
        assert_eq!(status.queue.len(), 2);
        assert_eq!(status.playback.seconds, 30.5);
        assert_eq!(status.playback.duration, Some(240.0));
        assert!(status.playback.can_pause && !status.playback.can_seek);
        assert_eq!(status.error, None);
        assert_eq!(status.control_error.as_deref(), Some("seek refused"));
    }
}
