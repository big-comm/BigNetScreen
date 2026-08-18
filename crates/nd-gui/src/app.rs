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
use crate::tr;

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
}

#[derive(Debug)]
pub enum AppMsg {
    Navigate(Page),
    /// Start streaming to this receiver.
    Cast(String),
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
    /// Send files to this receiver (from the devices page).
    MediaTarget(String),
    About,
}

#[derive(Debug)]
pub enum AppCmd {
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
    /// A cast session ended.
    CastFinished {
        id: String,
        error: Option<String>,
    },
    /// The measured round trip to the receiver, or `None` for no answer.
    LinkMeasured(Option<Duration>),
    /// A media session started, or failed to.
    ///
    /// Carried in a wrapper because a live session owns a socket and a task,
    /// neither of which has anything sensible to print.
    MediaStarted(StartedMedia),
}

/// The outcome of starting a media session.
pub struct StartedMedia(pub Result<Box<MediaSession>, String>);

impl std::fmt::Debug for StartedMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Ok(_) => write!(f, "StartedMedia(running)"),
            Err(err) => write!(f, "StartedMedia({err})"),
        }
    }
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
            // Room for the sidebar, a list and the panel beside it. Below this
            // the split view folds the sidebar away rather than squeezing it.
            set_default_width: 1080,
            set_default_height: 720,
            set_width_request: 420,
            set_height_request: 480,

            #[name = "split"]
            adw::OverlaySplitView {
                set_max_sidebar_width: 260.0,
                set_min_sidebar_width: 220.0,

                #[wrap(Some)]
                // No header bar over the sidebar: an empty one only pushed the
                // name of the application a title bar's height down the page.
                // The identity block sits at the very top instead, wrapped in a
                // `WindowHandle` so that area still drags the window — which is
                // what the header bar was quietly providing.
                set_sidebar = &gtk::Box {
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
                            set_spacing: 6,
                            set_margin_all: 8,

                            gtk::Button {
                                set_icon_name: "help-about-symbolic",
                                set_tooltip_text: Some(&tr!("About BigNetScreen")),
                                connect_clicked => AppMsg::About,
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

                        pack_start = &gtk::ToggleButton {
                            set_icon_name: "sidebar-show-symbolic",
                            set_tooltip_text: Some(&tr!("Show the sidebar")),
                            #[watch]
                            set_active: split.shows_sidebar(),
                            connect_toggled[split] => move |button| {
                                split.set_show_sidebar(button.is_active());
                            },
                        },

                        pack_end = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some(&tr!("Scan again")),
                            #[watch]
                            set_sensitive: model.active_cast.is_none(),
                            connect_clicked => AppMsg::Rescan,
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
                HomeOutput::Cast { id, source } => AppMsg::CastWith(id, source),
                HomeOutput::SendMedia(id) => AppMsg::MediaTarget(id),
                HomeOutput::Stop => AppMsg::Stop,
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
            virtual_available: false,
            source_type: SourceType::Monitor,
            generation: 0,
            settings: current.clone(),
            media_session: None,
            measured: None,
            probing: false,
        };

        let widgets = view_output!();

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

        if current.auto_discovery {
            start_discovery(&sender, 0, current.protocol);
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
                self.registry.clear();
                self.order.clear();
                self.status = tr!("Searching…");
                start_discovery(&sender, self.generation, self.settings.protocol);
            }
            AppMsg::DismissIssues => self.issues.clear(),
            AppMsg::SetAutoDiscovery(on) => {
                self.settings.auto_discovery = on;
                settings::set(self.settings.clone());
                self.settings_page.emit(SettingsMsg::Reload);
                if on {
                    sender.input(AppMsg::Rescan);
                }
            }
            AppMsg::SettingsChanged(new) => {
                let protocol_changed = new.protocol != self.settings.protocol;
                self.settings = new;
                self.devices.emit(DevicesMsg::Devices(self.entries()));
                // Changing which protocols to look for is the one setting that
                // cannot wait for the next session: the list on screen is the
                // result of the old choice.
                if protocol_changed {
                    sender.input(AppMsg::Rescan);
                }
            }
            AppMsg::MediaTarget(id) => {
                // Chosen on the devices page: the media page opens with that
                // receiver already selected, because the person just said so.
                self.media.emit(MediaMsg::Choose(id));
                self.page = Page::Media;
            }
            AppMsg::SendMedia(files, target) => self.send_media(files, target, &sender),
            AppMsg::CancelMedia => {
                // Dropping the session stops playback and takes the file server
                // down with it.
                self.media_session = None;
                self.media.emit(MediaMsg::Status(None));
                self.refresh_status();
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
        _root: &Self::Root,
    ) {
        match message {
            AppCmd::Added(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let info = handle.0.info();
                let id = info.id.clone();
                match self.placement(&info.display_name, info.kind.is_castable()) {
                    Placement::Skip => {}
                    Placement::Replace(old_id) => {
                        self.registry.remove(&old_id);
                        if let Some(index) = self.order.iter().position(|i| i == &old_id) {
                            // Keeps the receiver where the person last saw it.
                            self.order[index] = id.clone();
                        }
                        self.registry.insert(id, handle.0);
                    }
                    Placement::Add => {
                        if !self.order.contains(&id) {
                            self.order.push(id.clone());
                        }
                        // `entry`: an mDNS re-resolve must not replace an
                        // instance that may be mid-session.
                        self.registry.entry(id).or_insert(handle.0);
                    }
                }
                self.searching = false;
                self.refresh_status();
                self.push_devices();
            }
            AppCmd::Updated(handle, generation) => {
                if generation != self.generation {
                    return;
                }
                let id = handle.0.info().id;
                if self.registry.contains_key(&id) {
                    self.push_devices();
                }
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
            AppCmd::PollStates => {
                self.push_devices();
                self.push_media_status();
                self.measure_link(&sender);
                start_state_poll(&sender);
            }
            AppCmd::LinkMeasured(round_trip) => {
                self.probing = false;
                self.measured = Some(round_trip);
            }
            AppCmd::CastFinished { id, error } => {
                if self.active_cast.as_deref() == Some(id.as_str()) {
                    self.active_cast = None;
                }
                // The measurement belonged to that session.
                self.measured = None;
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
            AppCmd::MediaStarted(StartedMedia(result)) => match result {
                Ok(session) => {
                    self.media_session = Some(*session);
                    self.status = tr!("Sending files…");
                }
                Err(err) => {
                    tracing::warn!(%err, "sending media failed");
                    self.status = err;
                }
            },
        }

        // Repaint. Everything above only changed the model.
        self.update_view(widgets, sender);
    }
}

/// What to do with a receiver that has just been discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    Add,
    /// Ignore it: the same device is already listed, in an equal or better way.
    Skip,
    /// Take the place of this receiver, which is the same device discovered
    /// through a protocol we cannot stream to.
    Replace(String),
}

impl AppModel {
    /// Decides where a newly discovered receiver goes.
    ///
    /// One piece of equipment often announces itself over more than one
    /// protocol — a Samsung projector shows up as AirPlay (which we only
    /// discover) *and* as Miracast (which we can stream to). Listing both puts
    /// the same name twice, one of them inert, and the person has to guess.
    ///
    /// The rule is deliberately narrow: merge only when the name matches
    /// exactly **and** one of the two is discovery-only. Two different devices
    /// sharing a name is unlikely, and even then nothing is lost, because the
    /// entry that gives way could not stream anyway.
    fn placement(&self, name: &str, castable: bool) -> Placement {
        for (id, sink) in &self.registry {
            let info = sink.info();
            if info.display_name != name {
                continue;
            }
            return match (info.kind.is_castable(), castable) {
                (false, true) => Placement::Replace(id.clone()),
                _ => Placement::Skip,
            };
        }
        Placement::Add
    }

    /// The receivers, in the order they were found.
    fn entries(&self) -> Vec<DeviceEntry> {
        self.order
            .iter()
            .filter_map(|id| {
                let sink = self.registry.get(id)?;
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
            if status.finished && status.error.is_none() {
                self.media_session = None;
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
        let sink = self.registry.get(id)?;
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
            .active_cast
            .as_ref()
            .and_then(|id| self.registry.get(id))
            .and_then(|sink| sink.link())
            .and_then(|link| link.endpoint)
        else {
            return;
        };
        self.probing = true;
        sender.oneshot_command(async move {
            // Spaced out rather than run on every poll: the poll is there to
            // keep the list fresh, and opening a connection to the receiver
            // two and a half times a second would be rude to its firmware.
            tokio::time::sleep(LINK_PROBE_INTERVAL).await;
            AppCmd::LinkMeasured(nd_net::probe::round_trip(endpoint).await)
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
        } else if count == 1 {
            tr!("1 receiver found")
        } else {
            format!("{count} {}", tr!("receivers found"))
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
        if self.media_session.is_some() {
            self.media_session = None;
            self.media.emit(MediaMsg::Status(None));
        }
        let Some(id) = self.active_cast.clone() else {
            self.refresh_status();
            return;
        };
        let Some(sink) = self.registry.get(&id).cloned() else {
            return;
        };
        tracing::info!(%id, "stopping the stream at the user's request");
        self.status = tr!("Stopping…");
        // `stop_stream` signals the session; the `begin_cast` command returns on
        // its own and emits `CastFinished`.
        sender.oneshot_command(async move {
            let _ = sink.stop_stream().await;
            AppCmd::PollStates
        });
    }

    fn begin_cast(&mut self, id: String, sender: &ComponentSender<Self>) {
        if self.active_cast.is_some() {
            self.status = tr!("A stream is already running");
            return;
        }
        let Some(sink) = self.registry.get(&id).cloned() else {
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
        self.active_cast = Some(id.clone());
        self.page = Page::Home;

        let source_type = self.source_type;
        sender.oneshot_command(async move {
            let error = run_cast(sink, source_type).await.err();
            AppCmd::CastFinished { id, error }
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
        sender: &ComponentSender<Self>,
    ) {
        let Some(sink) = self.registry.get(&target).cloned() else {
            self.status = tr!("That device is no longer available");
            return;
        };
        // Sending files and mirroring the screen are two things the receiver
        // cannot do at once. Saying so beats the picture vanishing with no
        // explanation.
        if self.active_cast.is_some() {
            self.status = tr!("Stop sharing your screen before sending files");
            return;
        }

        let info = sink.info();
        tracing::info!(name = %info.display_name, count = files.len(), "sending files");
        self.status = format!("{} · {}", info.display_name, tr!("Sending files…"));

        if info.kind == SinkKind::Chromecast {
            let Some(address) = info.address.as_ref().and_then(|a| a.parse::<IpAddr>().ok()) else {
                self.status = tr!("That device is no longer available");
                return;
            };
            let port = self.settings.port;
            let sender_name = self.settings.display_name();
            sender.oneshot_command(async move {
                let result = MediaSession::start(address, files, port, sender_name)
                    .await
                    .map(Box::new)
                    .map_err(|e| e.to_string());
                AppCmd::MediaStarted(StartedMedia(result))
            });
            return;
        }

        // The mirroring route. It is a cast session like any other, so the Stop
        // button, the status on the row and the audio guard all apply — the
        // only difference is what is being sent.
        self.active_cast = Some(target.clone());
        self.page = Page::Home;
        sender.oneshot_command(async move {
            let error = play_files_by_mirroring(sink, files).await.err();
            AppCmd::CastFinished { id: target, error }
        });
    }
}

/// Plays a queue of files to a receiver that can only be a screen.
///
/// One session per file, in order. Not one session for the queue: the pipeline
/// is built around a single decoder, and swapping the file inside a running
/// session would mean rebuilding it anyway — with the receiver watching the
/// picture disappear and come back regardless.
async fn play_files_by_mirroring(
    sink: Arc<dyn Sink>,
    files: Vec<MediaFile>,
) -> std::result::Result<(), String> {
    let _audio = nd_core::audio_state::AudioGuard::start();

    for file in files {
        let playback = nd_core::capture::MediaPlayback {
            path: file.path.clone(),
            kind: file.kind,
            title: file.title(),
        };
        let source = nd_core::capture::CaptureSource::media_file(playback, (1920, 1080));

        let playing = sink.start_stream(source);
        match file.kind {
            // A photograph has no end to reach: the session would sit on that
            // one frame until somebody pressed stop.
            nd_core::media::MediaKind::Photo => {
                let shown = tokio::time::timeout(
                    Duration::from_secs(nd_chromecast::media::PHOTO_SECONDS),
                    playing,
                )
                .await;
                let _ = sink.stop_stream().await;
                if let Ok(Err(err)) = shown {
                    return Err(err.to_string());
                }
            }
            // A film or a track ends on its own, and the session ends with it.
            _ => playing.await.map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}

/// Runs a cast session: screen capture plus receiver.
async fn run_cast(sink: Arc<dyn Sink>, source_type: SourceType) -> std::result::Result<(), String> {
    // Started before anything is captured and dropped after everything is torn
    // down. Sharing a screen records the default output's monitor, and the
    // *departure* of that recording makes an effects chain rebuild itself —
    // which is when the desktop's default output gets rewritten behind the
    // person's back. The guard puts it back.
    let _audio = nd_core::audio_state::AudioGuard::start();

    let backend = nd_capture::select_backend_for(source_type).await;
    let source = backend
        .start(source_type)
        .await
        .map_err(|e| e.to_string())?;

    let result = sink.start_stream(source).await.map_err(|e| e.to_string());
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
fn start_discovery(sender: &ComponentSender<AppModel>, generation: u64, protocol: Protocol) {
    sender.command(move |out, shutdown| {
        shutdown
            .register(async move { run_discovery(out, generation, protocol).await })
            .drop_on_shutdown()
    });

    sender.oneshot_command(async move {
        tokio::time::sleep(EMPTY_HINT_AFTER).await;
        AppCmd::SearchTimedOut(generation)
    });
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
    dialog.present(Some(root));
}

#[cfg(test)]
mod tests {
    use super::*;

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
