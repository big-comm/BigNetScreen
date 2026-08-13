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

use super::{latency_hint, DeviceEntry, Page, SessionInfo};
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

pub struct HomePage {
    devices: FactoryVecDeque<DeviceRow>,
    /// The list of "what to share" options.
    share_list: gtk::ListBox,
    /// The switch in front of each capture mode, so the selection can be shown
    /// and — since only one mode can be in force — the others turned off.
    mode_switches: Vec<(SourceType, gtk::Switch)>,
    /// The one row whose availability depends on the desktop.
    virtual_row: adw::ActionRow,
    /// What is selected right now.
    source: SourceType,
    /// Set while the switches are being brought in line with `source`, so that
    /// writing to a switch does not read back as the person flipping it.
    settling: bool,
    session: Option<SessionInfo>,
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
    /// The capture mode in force, as the root component sees it.
    Source(SourceType),
    /// Put the switches back in line with the selection.
    RefreshModes,
    /// A row was clicked.
    Activated(String),
    Share(SourceType),
    OpenMedia,
    Stop,
}

#[derive(Debug)]
pub enum HomeOutput {
    /// Start streaming to this receiver.
    Cast(String),
    /// Change what will be captured.
    Source(SourceType),
    Stop,
    Navigate(Page),
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
                    set_label: &tr!("Ready to share"),
                    set_xalign: 0.0,
                    add_css_class: "page-title",
                },
                gtk::Label {
                    set_label: &tr!("Choose what to share, then pick a device to send it to."),
                    set_xalign: 0.0,
                    set_margin_bottom: 18,
                    add_css_class: "page-subtitle",
                },

                // Two columns on a wide window, stacked on a narrow one. The
                // right-hand column is about a running session, so on a narrow
                // window it belongs *above* the lists: it is the thing the
                // person came back to the window to look at.
                gtk::Box {
                    set_spacing: 18,
                    set_orientation: gtk::Orientation::Horizontal,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,

                        // What to share comes first, and the device second,
                        // because that is the order of the decision: *what* is
                        // being sent is a property of the person's own screen,
                        // and it holds whichever receiver they end up choosing.
                        gtk::Label {
                            set_label: &tr!("What to share"),
                            set_xalign: 0.0,
                            add_css_class: "section-heading",
                        },

                        #[local_ref]
                        share_list -> gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            set_valign: gtk::Align::Start,
                            add_css_class: "boxed-list",
                            // The rows are built in `init`: each carries a
                            // message of its own, which the view macro cannot
                            // express as cleanly as a loop can.
                        },

                        // Said here, not on a page of its own: this only
                        // matters at the moment someone picks "a window", and
                        // a fact kept somewhere else is a fact nobody reads.
                        gtk::Label {
                            #[watch]
                            set_label: &tr!(
                                "Your desktop will ask which window. Only visible windows can \
                                 be shared, and closing the window ends the session."
                            ),
                            #[watch]
                            set_visible: model.source == SourceType::Window,
                            set_xalign: 0.0,
                            set_wrap: true,
                            set_margin_top: 8,
                            add_css_class: "dim-label",
                        },

                        gtk::Label {
                            set_label: &tr!("Then pick a device"),
                            set_xalign: 0.0,
                            set_margin_top: 18,
                            add_css_class: "section-heading",
                        },

                        #[local_ref]
                        device_list -> gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            set_valign: gtk::Align::Start,
                            add_css_class: "boxed-list",
                        },
                    },

                    // The right-hand column.
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

                        // Only shown while a session is running: "excellent
                        // quality" next to nothing at all would be a claim
                        // about a link that has not been used yet.
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
                                    // Hidden where nothing can be measured:
                                    // four empty bars would read as "no
                                    // signal" rather than "not measured".
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
                    DeviceRowOutput::Activated(id) => HomeMsg::Activated(id),
                });

        // The four ways to share. Built here rather than in the view macro:
        // each row carries a different message, and a loop says that once.
        //
        // The three capture modes carry a switch; "Media" does not, because it
        // is not a mode — it opens another page.
        let share_list = gtk::ListBox::new();
        let mut mode_switches = Vec::new();
        let mut virtual_row = None;
        for (icon, colour, title, subtitle, mode) in [
            (
                "video-display-symbolic",
                "screen",
                tr!("Whole screen"),
                tr!("Share everything on your screen"),
                Some(SourceType::Monitor),
            ),
            (
                "window-new-symbolic",
                "window",
                tr!("A window"),
                tr!("Choose one window to share"),
                Some(SourceType::Window),
            ),
            (
                "video-joined-displays-symbolic",
                "virtual",
                tr!("A new screen"),
                tr!("Create an extra screen just for sharing"),
                Some(SourceType::Virtual),
            ),
            (
                "folder-videos-symbolic",
                "media",
                tr!("Media"),
                tr!("Send photos, videos and music"),
                None,
            ),
        ] {
            let row = adw::ActionRow::builder()
                .title(&title)
                .subtitle(&subtitle)
                .activatable(true)
                .build();

            match mode {
                Some(mode) => {
                    let toggle = gtk::Switch::builder()
                        .valign(gtk::Align::Center)
                        .active(mode == SourceType::Monitor)
                        .build();
                    let input = sender.input_sender().clone();
                    // Only switching *on* carries a decision. Switching a mode
                    // off would leave nothing selected, so that is answered by
                    // putting the switches back as they were.
                    toggle.connect_state_set(move |_, on| {
                        input.emit(if on {
                            HomeMsg::Share(mode)
                        } else {
                            HomeMsg::RefreshModes
                        });
                        gtk::glib::Propagation::Proceed
                    });
                    // In front of the icon, so the row says "this one is on"
                    // before it says what it is.
                    row.add_prefix(&toggle);
                    // The whole row selects the mode as well: aiming at a
                    // switch is fussy when the label beside it means the same.
                    let input = sender.input_sender().clone();
                    row.connect_activated(move |_| input.emit(HomeMsg::Share(mode)));
                    mode_switches.push((mode, toggle));
                    if colour == "virtual" {
                        virtual_row = Some(row.clone());
                    }
                }
                None => {
                    row.add_suffix(&gtk::Image::builder().icon_name("go-next-symbolic").build());
                    let input = sender.input_sender().clone();
                    row.connect_activated(move |_| input.emit(HomeMsg::OpenMedia));
                }
            }
            row.add_prefix(&mode_icon(icon, colour));
            share_list.append(&row);
        }

        let model = HomePage {
            devices,
            share_list: share_list.clone(),
            mode_switches,
            virtual_row: virtual_row.expect("the virtual screen row is always built"),
            source: SourceType::Monitor,
            settling: false,
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
        let share_list = &model.share_list;
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
                // Kept in place but insensitive, with the reason in a tooltip.
                // Hiding it would leave the person wondering whether the
                // feature exists at all.
                self.virtual_row.set_sensitive(available);
                if !available {
                    self.virtual_row.set_tooltip_text(Some(&tr!(
                        "This desktop cannot create an extra screen (it needs GNOME running natively)"
                    )));
                }
            }
            HomeMsg::Activated(id) => {
                sender.output(HomeOutput::Cast(id)).ok();
            }
            HomeMsg::Share(source) => {
                // Echoes of the switches being written to are not decisions.
                if self.settling {
                    return;
                }
                self.source = source;
                self.show_selection();
                sender.output(HomeOutput::Source(source)).ok();
            }
            HomeMsg::Source(source) => {
                self.source = source;
                self.show_selection();
            }
            HomeMsg::RefreshModes => {
                if !self.settling {
                    self.show_selection();
                }
            }
            HomeMsg::OpenMedia => {
                sender.output(HomeOutput::Navigate(Page::Media)).ok();
            }
            HomeMsg::Stop => {
                sender.output(HomeOutput::Stop).ok();
            }
        }
        self.update_view(widgets, sender);
    }
}

impl HomePage {
    /// Puts exactly one switch on: the mode in force.
    ///
    /// Written from the model rather than left to the widgets, because a switch
    /// the person turned off has to come back on if it was the selected one —
    /// there is no state in which nothing is selected.
    fn show_selection(&mut self) {
        self.settling = true;
        for (mode, toggle) in &self.mode_switches {
            let wanted = *mode == self.source;
            if toggle.is_active() != wanted {
                toggle.set_active(wanted);
            }
        }
        self.settling = false;
    }

    fn session_field<F: Fn(&SessionInfo) -> String>(&self, f: F) -> String {
        self.session.as_ref().map(f).unwrap_or_default()
    }

    /// What is known about the link, measured rather than assumed.
    ///
    /// The round trip is a real measurement — the time to open a connection to
    /// the port the receiver is already listening on — so the word beside it
    /// ("Excellent", "Weak") stands for a number a person could check, not for
    /// a mood. What it deliberately does not claim is bandwidth: a link can
    /// answer in 8 ms and still not carry the picture.
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

    /// Is there a measurement to show for this session's protocol?
    fn measurable(&self) -> bool {
        self.session.as_ref().map(|s| s.measurable).unwrap_or(false)
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
///
/// Round-tripping a translated string is not something to be proud of, but the
/// alternative — carrying the `SinkKind` into every page — spreads the sink
/// type through the interface for one line of text.
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

/// The coloured square in front of a "what to share" row.
fn mode_icon(icon: &str, colour: &str) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(24)
        .css_classes(["share-icon", colour])
        .build()
}
