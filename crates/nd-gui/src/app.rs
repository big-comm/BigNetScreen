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
    SetAutoDiscovery(bool),
    SettingsChanged(Settings),
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
    /// The service answered and is ours to talk to.
    Connected(Box<Connection>),
    /// The service could not be reached, with the reason to show.
    Unreachable(String),
    /// A fresh reading of everything the service publishes.
    State(Box<ServiceState>),
    /// A call the service refused.
    Refused(String),
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
            // Keep navigation visible alongside compact page layouts.
            add_css_class: "bns-window",
            set_default_width: 1280,
            set_default_height: 860,
            set_width_request: 760,
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
                            set_button_label: Some(&tr!("Got it")),
                            connect_button_clicked => AppMsg::DismissIssues,
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
            },
        );

        let model = AppModel {
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
                // Closing the window no longer ends the session — that is the
                // point of the service. Nothing to wait for, so nothing to
                // keep the window open for.
                self.allow_close.set(true);
                root.close();
            }
            AppMsg::Navigate(page) => self.page = page,
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
                if self.ndi_installing.get() || !crate::ndi_setup::can_install() {
                    return;
                }
                self.ndi_installing.set(true);
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Installing NDI…")),
                    Some(&tr!("Authenticate when prompted. Downloading and building the package may take a few minutes. Keep the app open until installation finishes.")),
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
                self.status = tr!("Stopping…");
                self.call(&sender, |service| async move { service.stop().await });
            }
            AppMsg::Rescan => {
                self.dismissed.clear();
                self.status = tr!("Searching…");
                self.call(&sender, |service| async move { service.rescan().await });
            }
            AppMsg::DismissIssues => self.dismissed = self.state.issues.clone(),
            AppMsg::SetAutoDiscovery(on) => {
                self.settings.auto_discovery = on;
                settings::set(self.settings.clone());
                self.settings_page.emit(SettingsMsg::Reload);
                self.call(&sender, move |service| async move {
                    service.set_auto_discovery(on).await
                });
            }
            AppMsg::SettingsChanged(new) => {
                let looks_elsewhere = new.protocol != self.settings.protocol
                    || new.auto_discovery != self.settings.auto_discovery;
                self.settings = new;
                self.devices
                    .emit(DevicesMsg::SyncAutoDiscovery(self.settings.auto_discovery));
                // Everything else the service re-reads when a session starts,
                // which is when it matters. Discovery cannot wait for that:
                // the list on screen is the result of the old choice.
                //
                // Written here and now rather than left to the page's delayed
                // save, because the service reads the file: telling it to
                // reload before the bytes are there had it keep the old
                // answer. These two are a switch and a combo, so there is no
                // stream of values to coalesce.
                if looks_elsewhere {
                    let settings = self.settings.clone();
                    settings::persist(&settings);
                    self.call(
                        &sender,
                        |service| async move { service.reload_settings().await },
                    );
                }
            }
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
                self.call(&sender, move |service| async move {
                    service.send_media(&target, &paths).await
                });
            }
            AppMsg::ControlMedia(command) => {
                use nd_core::media::MediaCommand;
                let (name, value, path) = match command {
                    MediaCommand::TogglePause => ("toggle-pause", 0.0, PathBuf::new()),
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
        if widgets.nav.selected_row().map(|row| row.index()) != Some(index) {
            if let Some(row) = widgets.nav.row_at_index(index) {
                widgets.nav.select_row(Some(&row));
            }
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
                AppCmd::Connected(_) => "connected",
                AppCmd::Unreachable(_) => "unreachable",
                AppCmd::State(_) => "state",
                AppCmd::Refused(_) => "refused",
            },
            "command output arrived"
        );
        match message {
            AppCmd::NdiInstalled(result) => self.ndi_installed(result, &sender, root),
            AppCmd::Connected(connection) => {
                tracing::info!("connected to the session service");
                self.service = Some(connection.0);
            }
            AppCmd::Unreachable(reason) => {
                tracing::error!(%reason, "the session service could not be reached");
                self.service = None;
                self.status = tr!("The sharing service is not responding. Try again.");
            }
            AppCmd::Refused(reason) => self.status = describe_refusal(&reason),
            AppCmd::State(state) => {
                let ndi_missing = state.status.kind == "ndi-runtime-missing"
                    && self.state.status.kind != "ndi-runtime-missing";
                let was_streaming = !self.state.session.is_idle();
                self.state = *state;
                self.status = describe_status(&self.state.status, &self.state.session);
                self.home
                    .emit(HomeMsg::VirtualAvailable(self.state.virtual_available));
                self.home
                    .emit(HomeMsg::Searching(self.state.status.kind == "searching"));
                self.push_devices();
                self.push_media_status();
                if ndi_missing {
                    self.offer_ndi_install(&sender, root);
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
                Ok(()) => AppCmd::Refused(String::new()),
                Err(err) => AppCmd::Refused(err.to_string()),
            }
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        if self.is_busy() {
            self.status = tr!("A stream is already running");
            return;
        }
        let source = nd_service::engine::source_name(self.source_type).to_string();
        // The page a browser is shown is for a stranger on the network, not for
        // a client, so the service cannot write it: it has no locale. This does.
        let page_text = web_page_text();
        self.status = tr!("Connecting…");
        self.call(sender, move |service| async move {
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
        let round_trip = (session.round_trip_ms > 0).then_some(session.round_trip_ms);
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
            quality: round_trip
                .map(|ms| nd_net::probe::Quality::of(Some(Duration::from_millis(ms)))),
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
                    Some(&tr!("Installation failed or authentication was cancelled. You can try again or install the library manually.")),
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

    fn offer_ndi_install(&self, sender: &ComponentSender<Self>, root: &adw::ApplicationWindow) {
        let can_install = crate::ndi_setup::can_install();
        let body = if can_install {
            tr!("NDI needs a proprietary library that is not included with BigNetScreen. Install ndi-sdk from the AUR and the required build tools? Your system will ask for administrator authentication. Other sharing methods work without it.")
        } else {
            tr!("NDI needs a proprietary library that is not included with BigNetScreen. Install the NDI 5 or 6 runtime for your distribution, then restart the app. Other sharing methods work without it.")
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

/// Connects to the service and then follows everything it publishes.
///
/// One subscription, not six: `PropertiesChanged` fires for the whole
/// interface, and `zbus` keeps the proxy's properties cached from those same
/// signals, so re-reading all of them afterwards costs no round trip.
fn follow_the_service(sender: &ComponentSender<AppModel>) {
    sender.command(|out, shutdown| {
        shutdown
            .register(async move {
                let connection = match zbus::Connection::session().await {
                    Ok(connection) => connection,
                    Err(err) => {
                        let _ = out.send(AppCmd::Unreachable(err.to_string()));
                        return;
                    }
                };
                // Property caching off. With it on there is no way to read
                // the cache that is both correct and safe: a second
                // subscription to `PropertiesChanged` races the update and
                // returns the state before it, and subscribing through the
                // proxy's own streams deadlocks the first read — six streams
                // registered as listeners and none of them polled while the
                // read waits on the cache they feed. Uncached, every read is
                // a call to the service and says what is true now. The bus is
                // local and the service only publishes when something moved.
                let service = match ServiceProxy::builder(&connection)
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                {
                    Ok(service) => service,
                    Err(err) => {
                        let _ = out.send(AppCmd::Unreachable(err.to_string()));
                        return;
                    }
                };
                let properties = match zbus::fdo::PropertiesProxy::builder(&connection)
                    .destination(nd_service::BUS_NAME)
                    .and_then(|builder| builder.path(nd_service::OBJECT_PATH))
                {
                    Ok(builder) => match builder.build().await {
                        Ok(properties) => properties,
                        Err(err) => {
                            let _ = out.send(AppCmd::Unreachable(err.to_string()));
                            return;
                        }
                    },
                    Err(err) => {
                        let _ = out.send(AppCmd::Unreachable(err.to_string()));
                        return;
                    }
                };
                let mut changes = match properties.receive_properties_changed().await {
                    Ok(changes) => changes,
                    Err(err) => {
                        let _ = out.send(AppCmd::Unreachable(err.to_string()));
                        return;
                    }
                };

                let _ = out.send(AppCmd::Connected(Box::new(Connection(service.clone()))));
                if let Some(state) = read_state(&service).await {
                    let _ = out.send(AppCmd::State(Box::new(state)));
                }
                while changes.next().await.is_some() {
                    if let Some(state) = read_state(&service).await {
                        if out.send(AppCmd::State(Box::new(state))).is_err() {
                            break;
                        }
                    }
                }
            })
            .drop_on_shutdown()
    });
}

async fn read_state(service: &ServiceProxy<'_>) -> Option<ServiceState> {
    /// Says which property went wrong instead of returning a bare `None`.
    ///
    /// The first version used `?` on each read, so a single failing property
    /// produced a window with no state and a log with nothing in it at all.
    macro_rules! read {
        ($name:literal, $call:expr) => {
            match tokio::time::timeout(std::time::Duration::from_secs(5), $call).await {
                Err(_) => {
                    tracing::warn!(property = $name, "the service did not answer in time");
                    return None;
                }
                Ok(answer) => match answer {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(property = $name, %err, "the service would not answer");
                        return None;
                    }
                },
            }
        };
    }
    Some(ServiceState {
        receivers: read!("Receivers", service.receivers()),
        session: read!("Session", service.session()),
        status: read!("Status", service.status()),
        issues: read!("Issues", service.issues()),
        media: read!("Media", service.media()),
        virtual_available: read!("VirtualAvailable", service.virtual_available()),
    })
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
        _ => reason.to_string(),
    }
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
        return format!("{}: {reason}", tr!("Miracast unavailable"));
    }
    if provider == "mdns" {
        return format!("{}: {reason}", tr!("Local network discovery unavailable"));
    }
    reason.to_string()
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
        // Anything we have no sentence for is passed through, not dropped.
        assert_eq!(describe_refusal("something odd"), "something odd");
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
