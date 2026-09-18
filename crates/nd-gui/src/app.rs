//! The window: a sidebar, a page for each thing the application does, and the
//! state that outlives any one of them.
//!
//! The pages are components in [`crate::pages`]; this module is what they have
//! in common. It owns the receivers that discovery finds, the running session
//! and the preferences, and it is the only place a cast is started or stopped.
//! Pages ask; this decides.
//!
//! Design points worth knowing:
//!
//! - **failures are visible**: a protocol that will not start becomes a banner
//!   saying why, not a `warn` in a log with the window stuck on "Searching…";
//! - **an existing receiver is updated, never replaced**: swapping the `Arc`
//!   mid-session would lose the connection that is running on it;
//! - **the pages are told, they do not ask**: every poll pushes the current list
//!   of receivers to whichever pages show one, so two parts of the window
//!   cannot disagree about what is connected;
//! - all text goes through `tr!()` (gettext).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_chromecast::file_server::MediaFile;
use nd_chromecast::media::{MediaSession, MediaStatus};
use nd_chromecast::MdnsProvider;
use nd_core::capture::SourceType;
use nd_core::meta::MetaProvider;
use nd_core::provider::{DiscoveryEvent, Provider};
use nd_core::settings::{self, Protocol, Settings};
use nd_core::sink::{Sink, SinkKind};
use nd_wfd::WfdP2pProvider;

use crate::pages::devices::{DevicesMsg, DevicesOutput, DevicesPage};
use crate::pages::home::{HomeMsg, HomeOutput, HomePage};
use crate::pages::media::{MediaMsg, MediaOutput, MediaPage};
use crate::pages::settings::{SettingsMsg, SettingsOutput, SettingsPage};
use crate::pages::{DeviceEntry, Page, SessionInfo};
use crate::{tr, tr_n};

/// After this long with no receiver, the window stops saying "searching".
const EMPTY_HINT_AFTER: Duration = Duration::from_secs(12);
/// How often the state of the receivers is re-read and pushed to the pages.
const STATE_POLL: Duration = Duration::from_millis(400);
/// How long to wait between measurements of the link.
///
/// Long enough to be unnoticeable to the receiver, short enough that the card
/// reflects a Wi-Fi that has just got worse.
const LINK_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// Wrapper for carrying an `Arc<dyn Sink>` as a `CommandOutput`.
#[derive(Clone)]
pub struct DiscoveredSink(pub Arc<dyn Sink>);

impl std::fmt::Debug for DiscoveredSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DiscoveredSink({})", self.0.info().id)
    }
}

/// A banner about a protocol that could not start.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderIssue {
    provider: &'static str,
    reason: String,
}

pub struct AppModel {
    home: Controller<HomePage>,
    devices: Controller<DevicesPage>,
    media: Controller<MediaPage>,
    settings_page: Controller<SettingsPage>,

    page: Page,
    /// Receivers found, by id.
    registry: HashMap<String, Arc<dyn Sink>>,
    /// The order they were found in, so the list does not shuffle on each poll.
    order: Vec<String>,
    status: String,
    issues: Vec<ProviderIssue>,
    /// Is it still worth saying "searching"?
    searching: bool,
    /// The running cast (the receiver's id).
    active_cast: Option<String>,
    active_sink: Option<Arc<dyn Sink>>,
    cast_cancel: Option<tokio::sync::watch::Sender<bool>>,
    discovery: Option<futures::future::AbortHandle>,
    /// Is a virtual monitor available on this desktop?
    virtual_available: bool,
    /// What will be captured when a receiver is picked.
    source_type: SourceType,
    /// The discovery generation: discards events from a previous run.
    generation: u64,
    settings: Settings,
    /// The receiver files are being sent to, and the session doing it.
    media_session: Option<MediaSession>,
    /// The last measurement of the link in the running session.
    ///
    /// Two levels on purpose: the outer `None` means *not measured yet* (the
    /// card says "measuring"), the inner `None` means *the receiver did not
    /// answer* (the card says so). One `Option` would have to call one of those
    /// the other.
    measured: Option<Option<Duration>>,
    /// Is a measurement in flight? One at a time: the probe opens a connection
    /// to the receiver, and stacking them up would be rude to firmware that
    /// accepts few.
    probing: bool,
    operation_generation: u64,
    ndi_installing: std::rc::Rc<std::cell::Cell<bool>>,
    ndi_install_dialog: Option<adw::AlertDialog>,
}

#[derive(Debug)]
pub enum AppMsg {
    Navigate(Page),
    /// Start streaming to this receiver.
    Cast(String),
    PublishNdi(SourceType),
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

#[derive(Debug)]
pub enum AppCmd {
    NdiInstalled(std::result::Result<(), String>),
    Added(DiscoveredSink, u64),
    Updated(DiscoveredSink, u64),
    Removed(String, u64),
    ProviderUnavailable {
        provider: &'static str,
        reason: String,
        generation: u64,
    },
    ProviderReady {
        provider: &'static str,
        generation: u64,
    },
    /// The "searching" deadline elapsed.
    SearchTimedOut(u64),
    /// The result of probing for virtual monitor support.
    VirtualSupported(bool),
    /// The periodic re-read of the receivers' state.
    PollStates,
    Refresh,
    /// A cast session ended.
    CastFinished {
        id: String,
        error: Option<String>,
        ndi_runtime_unavailable: bool,
    },
    /// The measured round trip to the receiver, or `None` for no answer.
    LinkMeasured(u64, Option<Duration>),
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
                            set_visible: model.active_cast.is_some(),

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
                                set_label: &model.session_info()
                                    .map(|s| s.name)
                                    .unwrap_or_default(),
                                set_xalign: 0.0,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "heading",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.session_info()
                                    .map(|s| s.address)
                                    .unwrap_or_default(),
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
                            set_visible: model.active_cast.is_some()
                                || model.media_session.is_some(),
                            connect_clicked => AppMsg::Stop,
                        },
                    },

                    #[wrap(Some)]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,

                        adw::Banner {
                            #[watch]
                            set_revealed: !model.issues.is_empty(),
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
                // The home page now names both halves of the decision in one
                // message: who receives, and what they receive.
                HomeOutput::PublishNdi(source) => AppMsg::PublishNdi(source),
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

        let mut model = AppModel {
            home,
            devices,
            media,
            settings_page,
            page: Page::Home,
            registry: HashMap::new(),
            order: Vec::new(),
            status: if current.auto_discovery {
                tr!("Searching…")
            } else {
                tr!("Automatic discovery is off")
            },
            issues: Vec::new(),
            searching: current.auto_discovery,
            active_cast: None,
            active_sink: None,
            cast_cancel: None,
            discovery: None,
            virtual_available: false,
            source_type: SourceType::Monitor,
            generation: 0,
            settings: current.clone(),
            media_session: None,
            measured: None,
            probing: false,
            operation_generation: 0,
            ndi_installing: Default::default(),
            ndi_install_dialog: None,
        };

        let widgets = view_output!();

        let ndi_installing = model.ndi_installing.clone();
        root.connect_close_request(move |_| {
            if ndi_installing.get() {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
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
        if current.auto_discovery {
            model.discovery = Some(start_discovery(&sender, 0, current.protocol));
        }
        start_state_poll(&sender);
        probe_virtual_support(&sender);

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
            AppMsg::Stop => self.stop_everything(&sender),
            AppMsg::Rescan => {
                self.generation += 1;
                self.issues.clear();
                self.searching = true;
                self.registry
                    .retain(|id, _| self.active_cast.as_ref() == Some(id));
                self.order
                    .retain(|id| self.active_cast.as_ref() == Some(id));
                self.status = tr!("Searching…");
                if let Some(previous) = self.discovery.take() {
                    previous.abort();
                }
                self.discovery = Some(start_discovery(
                    &sender,
                    self.generation,
                    self.settings.protocol,
                ));
            }
            AppMsg::DismissIssues => self.issues.clear(),
            AppMsg::SetAutoDiscovery(on) => {
                self.settings.auto_discovery = on;
                settings::set(self.settings.clone());
                self.settings_page.emit(SettingsMsg::Reload);
                if on {
                    sender.input(AppMsg::Rescan);
                } else {
                    if let Some(task) = self.discovery.take() {
                        task.abort();
                    }
                    self.generation += 1;
                    self.searching = false;
                    self.refresh_status();
                }
            }
            AppMsg::SettingsChanged(new) => {
                let protocol_changed = new.protocol != self.settings.protocol;
                let discovery_changed = new.auto_discovery != self.settings.auto_discovery;
                self.settings = new;
                self.devices
                    .emit(DevicesMsg::SyncAutoDiscovery(self.settings.auto_discovery));
                self.devices.emit(DevicesMsg::Devices(self.entries()));
                // Changing which protocols to look for is the one setting that
                // cannot wait for the next session: the list on screen is the
                // result of the old choice.
                if discovery_changed {
                    sender.input(AppMsg::SetAutoDiscovery(self.settings.auto_discovery));
                } else if protocol_changed {
                    if self.settings.auto_discovery {
                        sender.input(AppMsg::Rescan);
                    } else if let Some(task) = self.discovery.take() {
                        task.abort();
                        self.generation += 1;
                    }
                }
            }
            AppMsg::MediaTarget(id) => {
                // Chosen on the devices page: the media page opens with that
                // receiver already selected, because the person just said so.
                self.media.emit(MediaMsg::Choose(id));
                self.page = Page::Media;
            }
            AppMsg::SendMedia(files, target) => self.send_media(files, target, &sender),
            AppMsg::CancelMedia => self.stop_everything(&sender),
            AppMsg::ControlMedia(command) => {
                if let Some(session) = &self.media_session {
                    if let Err(err) = session.command(command) {
                        self.status = err.to_string();
                    }
                }
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
    /// at the end, nothing arriving from a command reaches the window. The
    /// symptom was Stop appearing to need two clicks: the first really did end
    /// the session, and the header went on saying "Stopping…" because the result
    /// of that work never repainted.
    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            AppCmd::NdiInstalled(result) => {
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
            AppCmd::Added(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let info = handle.0.info();
                let id = info.id.clone();
                if !self.order.contains(&id) {
                    self.order.push(id.clone());
                }
                self.registry.insert(id, handle.0);
                self.searching = false;
                self.refresh_status();
                self.push_devices();
            }
            AppCmd::Updated(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let id = handle.0.info().id;
                if !self.order.contains(&id) {
                    self.order.push(id.clone());
                }
                self.registry.insert(id, handle.0);
                self.push_devices();
            }
            AppCmd::Removed(id, generation) => {
                if generation != self.generation {
                    return;
                }
                // Never remove the receiver that is streaming: a momentary mDNS
                // dropout would take the running session's row away.
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    return;
                }
                self.registry.remove(&id);
                self.order.retain(|listed| listed != &id);
                self.refresh_status();
                self.push_devices();
            }
            AppCmd::ProviderUnavailable {
                provider,
                reason,
                generation,
            } => {
                if generation != self.generation {
                    return;
                }
                let issue = ProviderIssue { provider, reason };
                if !self.issues.contains(&issue) {
                    self.issues.push(issue);
                }
            }
            AppCmd::ProviderReady {
                provider,
                generation,
            } => {
                if generation != self.generation {
                    return;
                }
                self.issues.retain(|issue| issue.provider != provider);
            }
            AppCmd::VirtualSupported(available) => {
                tracing::info!(available, "virtual monitor support");
                self.virtual_available = available;
                self.home.emit(HomeMsg::VirtualAvailable(available));
            }
            AppCmd::SearchTimedOut(generation) => {
                if generation == self.generation && self.registry.is_empty() {
                    self.searching = false;
                    self.refresh_status();
                    self.home.emit(HomeMsg::Searching(false));
                }
            }
            AppCmd::Refresh => {
                self.push_devices();
                self.push_media_status();
            }
            AppCmd::PollStates => {
                self.push_devices();
                self.push_media_status();
                self.measure_link(&sender);
                start_state_poll(&sender);
            }
            AppCmd::LinkMeasured(generation, round_trip) => {
                if generation != self.operation_generation {
                    return;
                }
                self.probing = false;
                self.measured = Some(round_trip);
            }
            AppCmd::CastFinished {
                id,
                error,
                ndi_runtime_unavailable,
            } => {
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    self.active_cast = None;
                    self.active_sink = None;
                    self.cast_cancel = None;
                } else {
                    return;
                }
                // The measurement belonged to that session.
                self.measured = None;
                if ndi_runtime_unavailable {
                    let can_install = crate::ndi_setup::can_install();
                    let body = if can_install {
                        tr!("NDI needs a proprietary library that is not included with BigNetScreen. Install ndi-sdk from the AUR and the required build tools? Your system will ask for administrator authentication. Other sharing methods work without it.")
                    } else {
                        tr!("NDI needs a proprietary library that is not included with BigNetScreen. Install the NDI 5 or 6 runtime for your distribution, then restart the app. Other sharing methods work without it.")
                    };
                    let dialog =
                        adw::AlertDialog::new(Some(&tr!("NDI runtime required")), Some(&body));
                    dialog.add_response("close", &tr!("Close"));
                    dialog.set_close_response("close");
                    if can_install {
                        dialog.add_response("install", &tr!("Install"));
                        dialog
                            .set_response_appearance("install", adw::ResponseAppearance::Suggested);
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
                match error {
                    Some(err) => {
                        tracing::warn!(%id, %err, "cast session ended with an error");
                        self.status = err;
                    }
                    None => {
                        tracing::info!(%id, "cast session ended");
                        self.refresh_status();
                    }
                }
                self.push_devices();
            }
        }

        // Repaint. Everything above only changed the model.
        self.update_view(widgets, sender);
    }
}

impl AppModel {
    /// The receivers, in the order they were found.
    fn entries(&self) -> Vec<DeviceEntry> {
        self.order
            .iter()
            .filter_map(|id| {
                let sink = if self.active_cast.as_ref() == Some(id) {
                    self.active_sink.as_ref()
                } else {
                    self.registry.get(id)
                }?;
                Some(DeviceEntry::from_info(
                    &sink.info(),
                    sink.state(),
                    sink.error_message(),
                    sink.link(),
                ))
            })
            .collect()
    }

    /// Pushes the current list to every page that shows receivers.
    fn push_devices(&mut self) {
        let entries = self.entries();
        self.home.emit(HomeMsg::Devices(entries.clone()));
        self.home.emit(HomeMsg::Session(self.session_info()));
        self.devices.emit(DevicesMsg::Devices(entries));
        self.devices
            .emit(DevicesMsg::Active(self.active_cast.clone()));
        self.media.emit(MediaMsg::Targets(self.media_receivers()));
        // The selected capture mode lives here, so the switches on the home
        // page are told rather than left to remember on their own.
    }

    fn push_media_status(&mut self) {
        let status: Option<MediaStatus> = self.media_session.as_ref().map(|s| s.status());
        // A finished queue clears itself: leaving "playing 3 of 3" on screen
        // after the last file ended would be untrue within a second.
        if let Some(status) = &status {
            if status.finished {
                self.media_session = None;
                self.active_cast = None;
                self.active_sink = None;
                self.cast_cancel = None;
                self.measured = None;
                self.push_devices();
                if let Some(error) = &status.error {
                    self.status = error.clone();
                    self.media.emit(MediaMsg::Status(Some(status.clone())));
                    return;
                }
                self.media.emit(MediaMsg::Status(None));
                self.refresh_status();
                return;
            }
        }
        self.media.emit(MediaMsg::Status(status));
    }

    /// What is streaming right now.
    fn session_info(&self) -> Option<SessionInfo> {
        let id = self.active_cast.as_ref()?;
        let sink = if self.active_cast.as_ref() == Some(id) {
            self.active_sink.as_ref()
        } else {
            self.registry.get(id)
        }?;
        let info = sink.info();
        let link = sink.link();
        Some(SessionInfo {
            id: id.clone(),
            name: info.display_name.clone(),
            protocol: crate::pages::protocol_label(info.kind),
            address: info.address.clone().unwrap_or_default(),
            // The mode the two ends **agreed on**, which the protocol records
            // when it settles it. Empty until then: printing the preference
            // instead would show "1920 × 1080" for a link that came out at
            // 1280 × 720.
            mode: link.map(|l| l.describe()).unwrap_or_default(),
            state: sink.state(),
            measurable: link.and_then(|l| l.endpoint).is_some(),
            quality: self.measured.map(nd_net::probe::Quality::of),
            round_trip_ms: self.measured.flatten().map(|rtt| rtt.as_millis() as u64),
        })
    }

    /// The Chromecast that files would be sent to, if there is one.
    ///
    /// Preference order: the receiver picked on the devices page, then the one
    /// being streamed to, then the only Chromecast around. Miracast is never a
    /// candidate — it can be a screen, and knows nothing about playing a file.
    /// The receivers that can play a file, for the person to choose from.
    ///
    /// There is deliberately **no** "best guess" here. The previous version
    /// fell back to the first Chromecast it had found, and sent a song to a
    /// projector in another room that nobody had chosen — a mistake the person
    /// cannot undo from this side of the network. Casting to a device is
    /// visible to whoever is standing in front of it, so it takes an explicit
    /// choice, every time.
    ///
    /// Both protocols can be a destination, by different means: a Chromecast
    /// is handed the file and plays it itself, while a Miracast receiver is a
    /// screen, so the file is decoded here and sent as the picture.
    fn media_receivers(&self) -> Vec<(String, String)> {
        self.order
            .iter()
            .filter_map(|id| {
                let info = self.registry.get(id)?.info();
                match info.kind {
                    // Without a usable address there is nothing to send to.
                    SinkKind::Chromecast => {
                        info.address.as_ref()?.parse::<IpAddr>().ok()?;
                    }
                    // A Miracast receiver has no address until the group is
                    // formed, and needs none here: the session builds it.
                    SinkKind::WfdP2p | SinkKind::WfdMice => {}
                    _ => return None,
                }
                Some((id.clone(), info.display_name))
            })
            .collect()
    }

    /// Measures the link to the receiver, at most one probe at a time.
    ///
    /// Only while something is streaming, and only when the protocol has told
    /// us where the receiver is: a Miracast receiver is announced by MAC and
    /// has no address at all until the Wi-Fi Direct group exists.
    fn measure_link(&mut self, sender: &ComponentSender<Self>) {
        if self.probing {
            return;
        }
        let Some(endpoint) = self
            .active_sink
            .as_ref()
            .and_then(|sink| sink.link())
            .and_then(|link| link.endpoint)
        else {
            return;
        };
        self.probing = true;
        let generation = self.operation_generation;
        sender.oneshot_command(async move {
            // Spaced out rather than run on every poll: the poll is there to
            // keep the list fresh, and opening a connection to the receiver
            // two and a half times a second would be rude to its firmware.
            tokio::time::sleep(LINK_PROBE_INTERVAL).await;
            AppCmd::LinkMeasured(generation, nd_net::probe::round_trip(endpoint).await)
        });
    }

    fn refresh_status(&mut self) {
        if !self.settings.auto_discovery && self.registry.is_empty() {
            self.status = tr!("Automatic discovery is off");
            return;
        }
        let count = self.registry.len();
        self.status = if count == 0 {
            if self.searching {
                tr!("Searching…")
            } else {
                tr!("No receivers found")
            }
        } else {
            tr_n!("{} receiver found", "{} receivers found", count)
                .replace("{}", &count.to_string())
        };
    }

    fn issues_summary(&self) -> String {
        self.issues
            .iter()
            .map(|issue| issue.reason.clone())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Ends whatever is running: the stream, the file sending, or both.
    fn stop_everything(&mut self, sender: &ComponentSender<Self>) {
        if let Some(session) = &self.media_session {
            session.stop();
        }
        if let Some(cancel) = &self.cast_cancel {
            cancel.send_replace(true);
        }
        let Some(id) = self.active_cast.clone() else {
            self.refresh_status();
            return;
        };
        let Some(sink) = self.active_sink.clone() else {
            return;
        };
        tracing::info!(%id, "stopping the stream at the user's request");
        self.status = tr!("Stopping…");
        // `stop_stream` signals the session; the `begin_cast` command returns on
        // its own and emits `CastFinished`.
        sender.oneshot_command(async move {
            let _ = sink.stop_stream().await;
            AppCmd::Refresh
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        if self.active_cast.is_some() || self.media_session.is_some() {
            self.status = tr!("A stream is already running");
            return;
        }
        let sink: Arc<dyn Sink> = if id == nd_ndi::ID {
            Arc::new(nd_ndi::NdiPublisher::new(self.settings.display_name()))
        } else if let Some(sink) = self.registry.get(&id).cloned() {
            sink
        } else {
            self.status = tr!("That device is no longer available");
            return;
        };
        let info = sink.info();
        if !info.kind.is_castable() {
            self.status = format!(
                "{} {}",
                info.display_name,
                tr!("shows up in discovery only")
            );
            return;
        }

        tracing::info!(name = %info.display_name, "starting the stream");
        self.status = format!("{} · {}", info.display_name, tr!("Connecting…"));
        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        self.active_cast = Some(id.clone());
        self.active_sink = Some(sink.clone());
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        self.cast_cancel = Some(cancel);
        self.page = Page::Home;

        let source_type = self.source_type;
        sender.oneshot_command(async move {
            if id == nd_ndi::ID {
                let ready = tokio::select! {
                    result = tokio::task::spawn_blocking(nd_ndi::runtime_available) => result.unwrap_or(false),
                    _ = cancelled.changed() => return AppCmd::CastFinished {
                        id, error: None, ndi_runtime_unavailable: false,
                    },
                };
                if !*cancelled.borrow() && !ready {
                    return AppCmd::CastFinished {
                        id,
                        error: Some(tr!("NDI runtime required")),
                        ndi_runtime_unavailable: true,
                    };
                }
            }
            let error = run_cast(sink, source_type, cancelled).await.err();
            AppCmd::CastFinished { id, error, ndi_runtime_unavailable: false }
        });
    }

    /// Starts sending files to the receiver the person chose.
    ///
    /// The two protocols do genuinely different things here, and the difference
    /// is worth knowing before choosing:
    ///
    /// - a **Chromecast** is given a URL and plays the file itself. The quality
    ///   is the file's, and this computer only serves bytes;
    /// - a **Miracast** receiver is a screen and nothing else, so the file is
    ///   decoded here and streamed as the picture. It costs a re-encode, and
    ///   the playback stops if this computer does.
    fn send_media(
        &mut self,
        files: Vec<MediaFile>,
        target: String,
        _sender: &ComponentSender<Self>,
    ) {
        let Some(sink) = self.registry.get(&target).cloned() else {
            self.status = tr!("That device is no longer available");
            return;
        };
        // Sending files and mirroring the screen are two things the receiver
        // cannot do at once. Saying so beats the picture vanishing with no
        // explanation.
        if self.active_cast.is_some() || self.media_session.is_some() {
            self.status = tr!("Stop sharing your screen before sending files");
            return;
        }

        let info = sink.info();
        tracing::info!(name = %info.display_name, count = files.len(), "sending files");
        self.status = format!("{} · {}", info.display_name, tr!("Sending files…"));

        if info.kind == SinkKind::Chromecast {
            let Some(endpoint) = sink.control_endpoint() else {
                self.status = tr!("That device is no longer available");
                return;
            };
            match MediaSession::start(
                endpoint,
                files,
                self.settings.port,
                self.settings.display_name(),
            ) {
                Ok(session) => self.media_session = Some(session),
                Err(err) => self.status = err.to_string(),
            }
            self.push_media_status();
            return;
        }

        self.operation_generation += 1;
        self.probing = false;
        self.measured = None;
        match MediaSession::start_mirroring(sink.clone(), files) {
            Ok(session) => {
                self.active_cast = Some(target);
                self.active_sink = Some(sink);
                self.media_session = Some(session);
            }
            Err(err) => self.status = err.to_string(),
        }
        self.push_media_status();
    }
}

impl Drop for AppModel {
    fn drop(&mut self) {
        if let Some(task) = self.discovery.take() {
            task.abort();
        }
        if let Some(cancel) = &self.cast_cancel {
            cancel.send_replace(true);
        }
        if let Some(session) = &self.media_session {
            session.stop();
        }
    }
}

async fn stream_until_cancelled(
    sink: &Arc<dyn Sink>,
    source: nd_core::capture::CaptureSource,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    photo: Option<Duration>,
) -> std::result::Result<(), String> {
    if *cancel.borrow() {
        return Ok(());
    }
    let playing = sink.start_stream(source);
    tokio::pin!(playing);
    tokio::select! {
        biased;
        result = &mut playing => return result.map_err(|e| e.to_string()),
        _ = cancel.changed() => {},
        _ = async { match photo { Some(delay) => tokio::time::sleep(delay).await, None => std::future::pending().await } } => {},
    }
    let _ = sink.stop_stream().await;
    playing.await.map_err(|e| e.to_string())
}

async fn run_cast(
    sink: Arc<dyn Sink>,
    source_type: SourceType,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), String> {
    if *cancel.borrow() {
        return Ok(());
    }
    let backend = tokio::select! {
        backend = nd_capture::select_backend_for(source_type) => backend,
        _ = cancel.changed() => return Ok(()),
    };
    let result = async {
        let source = tokio::select! {
            result = backend.start(source_type) => result.map_err(|e| e.to_string())?,
            _ = cancel.changed() => return Ok(()),
        };
        stream_until_cancelled(&sink, source, &mut cancel, None).await
    }
    .await;
    let _ = backend.stop().await;
    result
}

/// Asks the capture backend, once, whether it can create a virtual monitor.
fn probe_virtual_support(sender: &ComponentSender<AppModel>) {
    sender.oneshot_command(async move {
        let backend = nd_capture::select_backend_for(SourceType::Virtual).await;
        let supported = backend.supported_sources().await;
        AppCmd::VirtualSupported(supported.contains(&SourceType::Virtual))
    });
}

fn start_state_poll(sender: &ComponentSender<AppModel>) {
    sender.oneshot_command(async move {
        tokio::time::sleep(STATE_POLL).await;
        AppCmd::PollStates
    });
}

/// Kicks off discovery and the empty-state deadline.
fn start_discovery(
    sender: &ComponentSender<AppModel>,
    generation: u64,
    protocol: Protocol,
) -> futures::future::AbortHandle {
    let (handle, registration) = futures::future::AbortHandle::new_pair();
    sender.command(move |out, shutdown| {
        shutdown
            .register(async move {
                let _ = futures::future::Abortable::new(
                    run_discovery(out, generation, protocol),
                    registration,
                )
                .await;
            })
            .drop_on_shutdown()
    });

    sender.oneshot_command(async move {
        tokio::time::sleep(EMPTY_HINT_AFTER).await;
        AppCmd::SearchTimedOut(generation)
    });
    handle
}

/// Runs discovery across the chosen providers and emits events to the window.
async fn run_discovery(out: relm4::Sender<AppCmd>, generation: u64, protocol: Protocol) {
    let mut providers: Vec<Arc<dyn Provider>> = Vec::new();

    // A preference for one protocol is honoured by **not starting** the other.
    // Filtering the list afterwards would leave the radio scanning for Wi-Fi
    // Direct peers during a Cast session, which is exactly the contention the
    // preference exists to avoid.
    if wants_network_discovery(protocol) {
        // A single mDNS daemon serves both Chromecast and AirPlay.
        match MdnsProvider::all_media_receivers() {
            Ok(provider) => providers.push(Arc::new(provider)),
            Err(err) => {
                let _ = out.send(AppCmd::ProviderUnavailable {
                    provider: "mdns",
                    reason: format!("{}: {err}", tr!("Local network discovery unavailable")),
                    generation,
                });
            }
        }
    }

    if wants_wifi_direct(protocol) {
        providers.push(Arc::new(WfdP2pProvider));
    }

    if std::env::var_os("NETWORK_DISPLAYS_DUMMY").is_some() {
        providers.push(Arc::new(nd_core::dummy::DummyProvider));
    }

    let meta = MetaProvider::new(providers);
    let mut stream = meta.discover().await;

    while let Some(event) = stream.next().await {
        let cmd = match event {
            DiscoveryEvent::Added(sink) => AppCmd::Added(DiscoveredSink(sink), generation),
            DiscoveryEvent::Updated(sink) => AppCmd::Updated(DiscoveredSink(sink), generation),
            DiscoveryEvent::Removed(id) => AppCmd::Removed(id, generation),
            DiscoveryEvent::ProviderUnavailable { provider, reason } => {
                AppCmd::ProviderUnavailable {
                    provider,
                    reason: friendly_reason(provider, &reason),
                    generation,
                }
            }
            DiscoveryEvent::ProviderReady { provider } => AppCmd::ProviderReady {
                provider,
                generation,
            },
        };
        if out.send(cmd).is_err() {
            break;
        }
    }
}

/// Should mDNS (Chromecast and AirPlay) be scanned for?
fn wants_network_discovery(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::Auto | Protocol::Cast)
}

/// Should Wi-Fi Direct (Miracast) be scanned for?
///
/// This one costs more than a socket: the radio leaves the access point to
/// scan for peers, which is why a preference for Cast has to *stop* it rather
/// than filter its results.
fn wants_wifi_direct(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::Auto | Protocol::Miracast)
}

/// Turns a provider's technical error into something a person can act on.
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
    reason.to_string()
}

fn show_about(root: &adw::ApplicationWindow) {
    let dialog = adw::AboutDialog::builder()
        .application_name("BigNetScreen")
        .application_icon("tv-symbolic")
        .version(crate::APP_VERSION)
        .developer_name("BigCommunity")
        .website("https://github.com/big-comm/BigNetScreen")
        .license_type(gtk::License::Gpl30)
        .comments(tr!("Share your screen with a TV, projector or Chromecast."))
        .build();
    dialog.add_link(
        &tr!("NDI setup"),
        "https://github.com/big-comm/BigNetScreen/blob/main/docs/ndi.md",
    );
    dialog.add_credit_section(
        Some("NDI"),
        &["NDI® is a registered trademark of Vizrt NDI AB."],
    );
    dialog.present(Some(root));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSink {
        started: std::sync::atomic::AtomicUsize,
        finished: std::sync::atomic::AtomicUsize,
        stop: tokio::sync::watch::Sender<bool>,
        ready: tokio::sync::Notify,
    }
    impl TestSink {
        fn new() -> Self {
            Self {
                started: Default::default(),
                finished: Default::default(),
                stop: tokio::sync::watch::channel(false).0,
                ready: Default::default(),
            }
        }
    }
    #[async_trait::async_trait]
    impl Sink for TestSink {
        fn info(&self) -> nd_core::sink::SinkInfo {
            nd_core::sink::SinkInfo {
                id: "test".into(),
                display_name: "Test".into(),
                kind: SinkKind::WfdP2p,
                address: None,
            }
        }
        fn state(&self) -> nd_core::sink::SinkState {
            nd_core::sink::SinkState::Streaming
        }
        async fn start_stream(&self, _: nd_core::capture::CaptureSource) -> nd_core::Result<()> {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut stop = self.stop.subscribe();
            self.ready.notify_one();
            if !*stop.borrow() {
                let _ = stop.changed().await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.finished
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn stop_stream(&self) -> nd_core::Result<()> {
            self.stop.send_replace(true);
            Ok(())
        }
    }
    fn test_media() -> MediaFile {
        MediaFile {
            path: "/unused/test.mp4".into(),
            kind: nd_core::media::MediaKind::Video,
            content_type: "video/mp4",
            size: 0,
        }
    }

    #[tokio::test]
    async fn stopping_a_playlist_does_not_start_the_next_item() {
        let sink = Arc::new(TestSink::new());
        let session =
            MediaSession::start_mirroring(sink.clone(), vec![test_media(), test_media()]).unwrap();
        sink.ready.notified().await;
        session.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(session.status().error.is_none());
        assert_eq!(sink.started.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(sink.finished.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn photo_timeout_waits_for_teardown() {
        let concrete = Arc::new(TestSink::new());
        let sink: Arc<dyn Sink> = concrete.clone();
        let (_cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let media = nd_core::capture::MediaPlayback {
            control: None,
            path: "/unused/photo.jpg".into(),
            kind: nd_core::media::MediaKind::Photo,
            title: "Test".into(),
        };
        let source = nd_core::capture::CaptureSource::media_file(media, (320, 240));
        stream_until_cancelled(
            &sink,
            source,
            &mut cancelled,
            Some(Duration::from_millis(10)),
        )
        .await
        .unwrap();
        assert_eq!(
            concrete.finished.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn choosing_one_protocol_stops_the_other_from_scanning() {
        // Not a filter over the results: the Wi-Fi Direct scan takes the radio
        // off the access point, which is the contention this preference exists
        // to avoid. Asking for Cast has to leave that scan unstarted.
        assert!(wants_network_discovery(Protocol::Cast));
        assert!(!wants_wifi_direct(Protocol::Cast));

        assert!(wants_wifi_direct(Protocol::Miracast));
        assert!(!wants_network_discovery(Protocol::Miracast));
    }

    #[test]
    fn the_default_looks_for_everything() {
        // Anything else would make a receiver invisible for a reason the
        // person never chose.
        assert!(wants_network_discovery(Protocol::Auto));
        assert!(wants_wifi_direct(Protocol::Auto));
    }
}
