//! "Ready to share": the receivers found, what to share, and what is running.
//!
//! The page answers three questions in the order a person asks them: *where can
//! I send this*, *what am I sending*, and *what is happening now*. The right
//! column only exists once something is running — an empty panel labelled
//! "connected device" while nothing is connected is furniture, not information.

use nd_core::capture::SourceType;
use nd_core::sink::SinkState;
use relm4::adw::{self, prelude::*};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;
use relm4::prelude::*;

use super::{latency_hint, DeviceEntry, SessionInfo};
use crate::tr;

// ----------------------------------------------------------------------------
// One receiver in the list
// ----------------------------------------------------------------------------

#[derive(Debug)]
pub struct DeviceRow {
    entry: DeviceEntry,
}

#[derive(Debug)]
pub enum DeviceRowMsg {
    Update(DeviceEntry),
}

#[derive(Debug)]
pub enum DeviceRowOutput {
    Activated(String),
}

#[relm4::factory(pub)]
impl FactoryComponent for DeviceRow {
    type Init = DeviceEntry;
    type Input = DeviceRowMsg;
    type Output = DeviceRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            #[watch]
            set_title: &self.entry.name,
            #[watch]
            set_subtitle: &self.subtitle(),
            #[watch]
            set_activatable: self.entry.castable && !self.entry.state.is_busy(),
            #[watch]
            set_sensitive: self.entry.castable,

            add_prefix = &gtk::Image {
                #[watch]
                set_icon_name: Some(self.entry.icon()),
            },

            add_suffix = &gtk::Label {
                add_css_class: "badge",
                // Centred rather than filled: without this the label takes the
                // whole height of the row and the pill touches the separators.
                set_valign: gtk::Align::Center,
                set_vexpand: false,
                #[watch]
                set_label: &self.entry.badge(),
                #[watch]
                set_css_classes: &["badge", self.entry.badge_class()],
                #[watch]
                set_visible: self.entry.state != SinkState::Disconnected
                    || !self.entry.castable,
            },

            add_suffix = &adw::Spinner {
                #[watch]
                set_visible: self.entry.state.is_busy(),
            },

            add_suffix = &gtk::Image {
                set_icon_name: Some("go-next-symbolic"),
                add_css_class: "dim-label",
                #[watch]
                set_visible: self.entry.castable && !self.entry.state.is_busy(),
            },

            connect_activated[sender, id = self.entry.id.clone()] => move |_| {
                sender.output(DeviceRowOutput::Activated(id.clone())).ok();
            },
        }
    }

    fn init_model(entry: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self { entry }
    }

    fn update(&mut self, message: Self::Input, _sender: FactorySender<Self>) {
        match message {
            DeviceRowMsg::Update(entry) => self.entry = entry,
        }
    }
}

impl DeviceRow {
    /// Protocol, address, and — when there is one — what the receiver is doing
    /// or why it failed.
    fn subtitle(&self) -> String {
        if self.entry.state == SinkState::Error && !self.entry.detail.is_empty() {
            return self.entry.detail.clone();
        }
        match latency_hint(self.entry.kind) {
            Some(hint) if self.entry.castable => format!("{} · {hint}", self.entry.subtitle()),
            _ => self.entry.subtitle(),
        }
    }
}

// ----------------------------------------------------------------------------
// The page
// ----------------------------------------------------------------------------

/// Which half of the decision the page is showing.
///
/// Two steps rather than one screen with everything on it. Choosing a receiver
/// and choosing what to send are not the same kind of decision — the first is
/// about the room, the second about this computer — and putting both in front
/// of someone at once made the page a form to fill in rather than a thing to
/// use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    /// Pick who receives.
    #[default]
    Device,
    /// Pick what to send to the receiver already chosen.
    Action,
}

pub struct HomePage {
    devices: FactoryVecDeque<DeviceRow>,
    step: Step,
    /// The receiver picked in the first step.
    chosen: Option<DeviceEntry>,
    session: Option<SessionInfo>,
    /// The "extra screen" button, whose availability depends on the desktop.
    virtual_button: gtk::Button,
    /// Is there any receiver at all in the list?
    empty: bool,
    /// Still worth saying "searching"?
    searching: bool,
    virtual_available: bool,
}

#[derive(Clone, Debug)]
pub enum HomeMsg {
    /// The full list of receivers, as the root component sees it.
    Devices(Vec<DeviceEntry>),
    /// What is streaming, if anything.
    Session(Option<SessionInfo>),
    Searching(bool),
    VirtualAvailable(bool),
    /// A receiver was picked: move on to what to send it.
    Selected(String),
    /// Back to the list of receivers.
    Back,
    /// Send this to the receiver already chosen.
    Act(SourceType),
    /// Go to the media page with the chosen receiver.
    SendMedia,
    Stop,
}

#[derive(Debug)]
pub enum HomeOutput {
    /// Start streaming this to this receiver.
    Cast {
        id: String,
        source: SourceType,
    },
    /// Open the media page with this receiver already selected.
    SendMedia(String),
    Stop,
}

#[relm4::component(pub)]
impl Component for HomePage {
    type Init = ();
    type Input = HomeMsg;
    type Output = HomeOutput;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_margin_all: 24,
                set_spacing: 6,

                gtk::Label {
                    #[watch]
                    set_label: &model.title(),
                    set_xalign: 0.0,
                    add_css_class: "page-title",
                },
                gtk::Label {
                    #[watch]
                    set_label: &model.subtitle(),
                    set_xalign: 0.0,
                    set_margin_bottom: 18,
                    set_wrap: true,
                    add_css_class: "page-subtitle",
                },

                gtk::Box {
                    set_spacing: 18,
                    set_orientation: gtk::Orientation::Horizontal,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,

                        // Step one: who receives.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 8,
                            #[watch]
                            set_visible: model.step == Step::Device,

                            #[local_ref]
                            device_list -> gtk::ListBox {
                                set_selection_mode: gtk::SelectionMode::None,
                                set_valign: gtk::Align::Start,
                                add_css_class: "boxed-list",
                            },
                        },

                        // Step two: what to send there.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 12,
                            #[watch]
                            set_visible: model.step == Step::Action,

                            gtk::Box {
                                set_spacing: 8,

                                gtk::Button {
                                    set_valign: gtk::Align::Center,
                                    connect_clicked => HomeMsg::Back,
                                    adw::ButtonContent {
                                        set_icon_name: "go-previous-symbolic",
                                        set_label: &tr!("Back"),
                                    },
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.chosen_name(),
                                    set_xalign: 0.0,
                                    set_hexpand: true,
                                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                                    add_css_class: "title-2",
                                },
                            },

                            #[local_ref]
                            action_grid -> gtk::FlowBox {
                                set_selection_mode: gtk::SelectionMode::None,
                                set_max_children_per_line: 2,
                                set_min_children_per_line: 1,
                                set_column_spacing: 12,
                                set_row_spacing: 12,
                                set_homogeneous: true,
                            },

                            gtk::Label {
                                #[watch]
                                set_label: &tr!(
                                    "Sharing a window opens your desktop’s own picker. Only \
                                     visible windows can be shared."
                                ),
                                set_xalign: 0.0,
                                set_wrap: true,
                                add_css_class: "dim-label",
                            },
                        },
                    },

                    // The right-hand column: what is running, and advice.
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 14,
                        set_width_request: 280,
                        set_valign: gtk::Align::Start,

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 6,
                            add_css_class: "hero-card",
                            #[watch]
                            set_visible: model.session.is_some(),

                            gtk::Image {
                                set_icon_name: Some("tv-symbolic"),
                                set_pixel_size: 52,
                                set_margin_bottom: 6,
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.session_field(|s| s.name.clone()),
                                add_css_class: "hero-name",
                                set_wrap: true,
                                set_justify: gtk::Justification::Center,
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.session_field(|s| {
                                    if s.address.is_empty() {
                                        s.protocol.clone()
                                    } else {
                                        format!("{} · {}", s.address, s.protocol)
                                    }
                                }),
                                add_css_class: "hero-detail",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.session_field(|s| s.mode.clone()),
                                #[watch]
                                set_visible: !model.session_field(|s| s.mode.clone()).is_empty(),
                                add_css_class: "hero-chip",
                                set_halign: gtk::Align::Center,
                                set_margin_top: 4,
                            },
                            gtk::Button {
                                set_label: &tr!("Disconnect"),
                                add_css_class: "destructive-action",
                                set_halign: gtk::Align::Center,
                                set_margin_top: 10,
                                connect_clicked => HomeMsg::Stop,
                            },
                        },

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 4,
                            add_css_class: "info-card",
                            #[watch]
                            set_visible: model.session.is_some(),

                            gtk::Box {
                                set_spacing: 8,

                                gtk::Label {
                                    #[watch]
                                    set_label: &if model.measurable() {
                                        tr!("Connection quality")
                                    } else {
                                        tr!("Connection")
                                    },
                                    set_hexpand: true,
                                    set_xalign: 0.0,
                                    add_css_class: "heading",
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &super::quality_bars(
                                        model.session.as_ref().and_then(|s| s.quality)
                                    ),
                                    #[watch]
                                    set_visible: model.measurable(),
                                    #[watch]
                                    set_css_classes: &[model.signal_class()],
                                },
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.connection_line(),
                                set_xalign: 0.0,
                                set_wrap: true,
                                add_css_class: "dim-label",
                            },
                        },

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 4,
                            add_css_class: "info-card",

                            gtk::Label {
                                set_label: &tr!("Tips"),
                                set_xalign: 0.0,
                                add_css_class: "heading",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &model.tip(),
                                set_xalign: 0.0,
                                set_wrap: true,
                                add_css_class: "dim-label",
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let devices =
            FactoryVecDeque::builder()
                .launch_default()
                .forward(sender.input_sender(), |output| match output {
                    DeviceRowOutput::Activated(id) => HomeMsg::Selected(id),
                });

        // The four things that can be sent, as buttons. Built here rather than
        // in the view macro because each carries a different message, and a
        // loop says that once.
        let action_grid = gtk::FlowBox::new();
        let mut virtual_button = None;
        for (icon, colour, title, subtitle, message) in [
            (
                "video-display-symbolic",
                "screen",
                tr!("Whole screen"),
                tr!("Everything on your screen"),
                HomeMsg::Act(SourceType::Monitor),
            ),
            (
                "window-new-symbolic",
                "window",
                tr!("A window"),
                tr!("One window only"),
                HomeMsg::Act(SourceType::Window),
            ),
            (
                "video-joined-displays-symbolic",
                "virtual",
                tr!("A new screen"),
                tr!("An extra desktop, just for this"),
                HomeMsg::Act(SourceType::Virtual),
            ),
            (
                "folder-videos-symbolic",
                "media",
                tr!("Send media"),
                tr!("Photos, videos and music"),
                HomeMsg::SendMedia,
            ),
        ] {
            let button = action_button(icon, colour, &title, &subtitle);
            let input = sender.input_sender().clone();
            let message = message.clone();
            button.connect_clicked(move |_| input.emit(message.clone()));
            if colour == "virtual" {
                virtual_button = Some(button.clone());
            }
            action_grid.append(&button);
        }

        let model = HomePage {
            devices,
            virtual_button: virtual_button.expect("the extra-screen button is always built"),
            step: Step::Device,
            chosen: None,
            session: None,
            empty: true,
            searching: true,
            virtual_available: false,
        };
        model
            .devices
            .widget()
            .set_placeholder(Some(&searching_placeholder()));

        let device_list = model.devices.widget();
        let action_grid = &action_grid;
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match message {
            HomeMsg::Devices(entries) => {
                self.empty = entries.is_empty();
                // The receiver being acted on may change state, or leave.
                if let Some(chosen) = &self.chosen {
                    match entries.iter().find(|e| e.id == chosen.id) {
                        Some(fresh) => self.chosen = Some(fresh.clone()),
                        None => {
                            // Gone from the network: there is nothing to act on
                            // any more, so the page goes back rather than
                            // offering buttons that would fail.
                            self.chosen = None;
                            self.step = Step::Device;
                        }
                    }
                }
                sync_rows(&mut self.devices, entries);
            }
            HomeMsg::Session(session) => self.session = session,
            HomeMsg::Searching(searching) => {
                self.searching = searching;
                self.devices.widget().set_placeholder(Some(&if searching {
                    searching_placeholder()
                } else {
                    nothing_found_placeholder()
                }));
            }
            HomeMsg::VirtualAvailable(available) => {
                self.virtual_available = available;
                // Left in place but insensitive: hiding it would leave the
                // person wondering whether the feature exists at all.
                self.virtual_button.set_sensitive(available);
                if !available {
                    self.virtual_button.set_tooltip_text(Some(&tr!(
                        "This desktop cannot create an extra screen (it needs GNOME running \
                         natively)"
                    )));
                }
            }
            HomeMsg::Selected(id) => {
                self.chosen = self
                    .devices
                    .iter()
                    .find(|row| row.entry.id == id)
                    .map(|row| row.entry.clone());
                if self.chosen.is_some() {
                    self.step = Step::Action;
                }
            }
            HomeMsg::Back => {
                self.step = Step::Device;
                self.chosen = None;
            }
            HomeMsg::Act(source) => {
                if let Some(chosen) = &self.chosen {
                    sender
                        .output(HomeOutput::Cast {
                            id: chosen.id.clone(),
                            source,
                        })
                        .ok();
                    // Back to the list: what happens next is a running session,
                    // and that is shown beside it.
                    self.step = Step::Device;
                }
            }
            HomeMsg::SendMedia => {
                if let Some(chosen) = &self.chosen {
                    sender.output(HomeOutput::SendMedia(chosen.id.clone())).ok();
                    self.step = Step::Device;
                }
            }
            HomeMsg::Stop => {
                sender.output(HomeOutput::Stop).ok();
            }
        }
        self.update_view(widgets, sender);
    }
}

impl HomePage {
    /// The heading, which follows the step.
    fn title(&self) -> String {
        match self.step {
            Step::Device => tr!("Ready to share"),
            Step::Action => tr!("What do you want to share?"),
        }
    }

    fn subtitle(&self) -> String {
        match self.step {
            Step::Device => tr!("Choose the device to send to."),
            Step::Action => self
                .chosen
                .as_ref()
                .map(|d| format!("{} · {}", d.name, d.subtitle()))
                .unwrap_or_default(),
        }
    }

    fn chosen_name(&self) -> String {
        self.chosen
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    fn session_field<F: Fn(&SessionInfo) -> String>(&self, f: F) -> String {
        self.session.as_ref().map(f).unwrap_or_default()
    }

    /// Is there a measurement to show for this session’s protocol?
    fn measurable(&self) -> bool {
        self.session.as_ref().map(|s| s.measurable).unwrap_or(false)
    }

    /// What is known about the link, measured rather than assumed.
    fn connection_line(&self) -> String {
        let Some(session) = &self.session else {
            return String::new();
        };
        let mut parts = Vec::new();
        if session.measurable {
            parts.push(super::quality_line(session));
        }
        let state = super::state_label(session.state);
        if !state.is_empty() {
            parts.push(state);
        }
        if let Some(hint) = latency_hint(sink_kind_of(&session.protocol)) {
            parts.push(hint);
        }
        parts.join(" · ")
    }

    /// The colour of the bars: green while the link is fine, amber when not.
    fn signal_class(&self) -> &'static str {
        use nd_net::probe::Quality;
        match self.session.as_ref().and_then(|s| s.quality) {
            Some(Quality::Excellent) | Some(Quality::Good) => "signal-good",
            _ => "signal-weak",
        }
    }

    fn tip(&self) -> String {
        if self.session.is_some() {
            return tr!(
                "Sound goes to the receiver and to this computer. Mute this computer if you \
                 hear an echo."
            );
        }
        if self.empty && !self.searching {
            return tr!(
                "Chromecasts show up on their own. A TV or projector only appears while it \
                 is in screen mirroring mode — on Fire TV, under Settings › Display and \
                 Sounds › Display Mirroring."
            );
        }
        tr!("Keep every device on the same Wi-Fi network for the best results.")
    }
}

/// Recovers the protocol from its label, for the latency hint.
fn sink_kind_of(protocol: &str) -> nd_core::sink::SinkKind {
    use nd_core::sink::SinkKind;
    if protocol == super::protocol_label(SinkKind::WfdP2p) {
        SinkKind::WfdP2p
    } else if protocol == super::protocol_label(SinkKind::Chromecast) {
        SinkKind::Chromecast
    } else {
        SinkKind::AirPlay
    }
}

/// One thing that can be sent, as a button rather than a row.
///
/// A row with a switch asked the person to set a mode and then go looking for
/// what applies it. A button is the verb itself: pressing it starts that.
fn action_button(icon: &str, colour: &str, title: &str, subtitle: &str) -> gtk::Button {
    let image = gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(24)
        .css_classes(["share-icon", colour])
        .build();

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    let name = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let detail = gtk::Label::builder()
        .label(subtitle)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .build();
    text.append(&name);
    text.append(&detail);

    let content = gtk::Box::builder().spacing(12).build();
    content.append(&image);
    content.append(&text);

    gtk::Button::builder()
        .child(&content)
        .css_classes(["action-tile"])
        .build()
}

/// Brings the factory in line with `entries`, in place.
///
/// Rebuilding the list on every poll would be simpler and would also make the
/// list flicker two and a half times a second, losing the row under the
/// pointer as the person is about to click it.
pub fn sync_rows(factory: &mut FactoryVecDeque<DeviceRow>, entries: Vec<DeviceEntry>) {
    let mut guard = factory.guard();
    let listed: Vec<String> = guard.iter().map(|row| row.entry.id.clone()).collect();

    // Drop rows for receivers that are gone, from the back so the indices of
    // the ones still to be examined do not shift.
    for (index, id) in listed.iter().enumerate().rev() {
        if !entries.iter().any(|e| &e.id == id) {
            guard.remove(index);
        }
    }
    for entry in entries {
        // The position is taken before touching the guard again: holding the
        // iterator across the update would borrow it twice.
        let existing = guard.iter().position(|row| row.entry.id == entry.id);
        match existing {
            Some(index) => {
                let unchanged =
                    guard.get(index).map(|row| row.entry.clone()) == Some(entry.clone());
                if !unchanged {
                    guard.send(index, DeviceRowMsg::Update(entry));
                }
            }
            None => {
                guard.push_back(entry);
            }
        }
    }
}

fn searching_placeholder() -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name("video-display-symbolic")
        .title(tr!("Looking for receivers…"))
        .build();
    page.add_css_class("compact");
    page
}

fn nothing_found_placeholder() -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name("video-display-symbolic")
        .title(tr!("No receivers found"))
        // Written "Display and Sounds", not "Display & Sounds": the description
        // is parsed as Pango markup, and a bare ampersand makes the whole
        // string fail to render.
        .description(tr!(
            "Chromecasts show up on their own. A TV or projector only appears while it is \
             in screen mirroring mode — on Fire TV, under Settings › Display and Sounds › \
             Display Mirroring."
        ))
        .build();
    page.add_css_class("compact");
    page
}
