//! "Ready to share": what is running, the receivers found, and what to share.
//!
//! A running session leads the page with its state, how to join it and Stop.
//! Otherwise the page asks *where can I send this*, then *what am I sending*.

use nd_core::capture::SourceType;
use relm4::adw::{self, prelude::*};
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;

use super::device_row::{DeviceRow, DeviceRowOutput, sync_rows};
use super::{DeviceEntry, SessionInfo, latency_hint};
use crate::tr;

/// Which half of the decision the page is showing: first the receiver, which
/// is about the room, then what to send, which is about this computer.
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
    audio_summary: String,
    /// The "extra screen" button, whose availability depends on the desktop.
    first_tile: gtk::Button,
    virtual_button: gtk::Button,
    media_button: gtk::Button,
    virtual_available: bool,
    /// The address the QR code on screen encodes, so the code is redrawn only
    /// when the address changes.
    qr_for: Option<String>,
}

#[derive(Clone, Debug)]
pub enum HomeMsg {
    /// The full list of receivers, as the root component sees it.
    Devices(Vec<DeviceEntry>),
    /// What is streaming, if anything.
    Session(Option<SessionInfo>),
    Discovery(crate::pages::Discovery),
    VirtualAvailable(bool),
    AudioSummary(String),
    AudioSettings,
    /// A receiver was picked: move on to what to send it.
    Selected(String),
    /// Back to the list of receivers.
    Back,
    /// Send this to the receiver already chosen.
    Act(SourceType),
    PublishNdi(SourceType),
    /// Share to web browsers on the network.
    PublishWeb(SourceType),
    /// Put the session's address on the clipboard (for OBS, a chat, a mail).
    CopyUrl,
    /// Go to the media page with the chosen receiver.
    SendMedia,
    Rescan,
    Stop,
}

#[derive(Debug)]
pub enum HomeOutput {
    AudioSettings,
    PublishNdi(SourceType),
    PublishWeb(SourceType),
    /// Start streaming this to this receiver.
    Cast {
        id: String,
        source: SourceType,
    },
    /// Open the media page with this receiver already selected.
    SendMedia(String),
    Rescan,
    Stop,
}

#[relm4::component(pub)]
impl Component for HomePage {
    type Init = ();
    type Input = HomeMsg;
    type Output = HomeOutput;
    type CommandOutput = ();

    view! {
        adw::BreakpointBin {
            set_width_request: 320,
            set_height_request: 360,
        #[wrap(Some)]
        set_child = &gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_margin_all: 28,
                add_css_class: "home-page",
                set_spacing: 6,

                gtk::Box {
                    add_css_class: "page-heading",
                    set_spacing: 18,
                    gtk::Image { set_icon_name: Some("video-display-symbolic"), set_pixel_size: 32, add_css_class: "page-icon", set_valign: gtk::Align::Center },
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical, set_spacing: 6, set_valign: gtk::Align::Center,
                        gtk::Label { #[watch] set_label: &model.title(), set_xalign: 0.0, set_wrap: true, add_css_class: "page-title" },
                        gtk::Label { #[watch] set_label: &model.subtitle(), set_xalign: 0.0, set_wrap: true, add_css_class: "page-subtitle" },
                    },
                },

                gtk::Button {
                    set_halign: gtk::Align::Start,
                    set_margin_bottom: 10,
                    #[watch]
                    set_visible: model.session.is_none(),
                    set_tooltip_text: Some(&tr!("Choose the sound to share")),
                    connect_clicked => HomeMsg::AudioSettings,
                    adw::ButtonContent {
                        set_icon_name: "audio-volume-high-symbolic",
                        #[watch]
                        set_label: &model.audio_summary,
                    },
                },

                // What is running, above the choices that would replace it.
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 14,
                    set_margin_bottom: 14,
                    #[watch]
                    set_visible: model.session.is_some(),

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
                        set_label: &model.session_field(|s| crate::pages::state_label(s.state)),
                        add_css_class: "hero-detail",
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

                    // How a receiver joins, for the sessions hosted here.
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 4,
                        set_margin_top: 12,
                        #[watch]
                        set_visible: model.access().is_some(),

                        gtk::Label {
                            set_label: &tr!("On the TV, phone or computer, open:"),
                            add_css_class: "hero-detail",
                            set_wrap: true,
                            set_justify: gtk::Justification::Center,
                        },
                        gtk::Box {
                            set_spacing: 6,
                            set_halign: gtk::Align::Center,
                            gtk::Label {
                                #[watch]
                                set_label: &model.access_field(|a| a.url.clone()),
                                add_css_class: "access-url",
                                set_selectable: true,
                                set_wrap: true,
                                set_justify: gtk::Justification::Center,
                            },
                            gtk::Button {
                                set_icon_name: "edit-copy-symbolic",
                                add_css_class: "flat",
                                set_valign: gtk::Align::Center,
                                set_tooltip_text: Some(&tr!("Copy the address")),
                                connect_clicked => HomeMsg::CopyUrl,
                            },
                        },
                        gtk::Label {
                            set_label: &tr!("and enter the PIN"),
                            add_css_class: "hero-detail",
                            set_margin_top: 6,
                        },
                        gtk::Label {
                            #[watch]
                            set_label: &model.access_field(|a| a.pin.clone()),
                            add_css_class: "access-pin",
                        },
                        gtk::Box {
                            add_css_class: "qr-frame",
                            set_halign: gtk::Align::Center,
                            set_margin_top: 8,
                            #[name = "qr_picture"]
                            gtk::Picture {
                                set_size_request: (168, 168),
                                set_can_shrink: true,
                            },
                        },
                    },
                    gtk::Button {
                        set_label: &tr!("Stop sharing"),
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
                                tr!("Network response")
                            } else {
                                tr!("Connection")
                            },
                            set_hexpand: true,
                            set_xalign: 0.0,
                            add_css_class: "heading",
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
                },

                gtk::Box {
                    set_spacing: 20,
                    set_orientation: gtk::Orientation::Vertical,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,

                        // Step one: who receives.
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 14,
                            add_css_class: "receivers-card",
                            #[watch]
                            set_visible: model.step == Step::Device,

                            #[name = "receivers_heading"]
                            gtk::Box {
                                set_spacing: 12,
                                gtk::Box {
                                    set_spacing: 10,
                                    set_hexpand: true,
                                    gtk::Label { set_label: &tr!("Receivers found"), set_xalign: 0.0, set_wrap: true, add_css_class: "heading" },
                                    gtk::Label { #[watch] set_label: &model.devices.len().to_string(), add_css_class: "receiver-count", set_valign: gtk::Align::Center },
                                },
                                #[name = "find_devices"]
                                gtk::Button {
                                    set_valign: gtk::Align::Center,
                                    #[watch] set_sensitive: model.session.is_none(),
                                    connect_clicked => HomeMsg::Rescan,
                                    adw::ButtonContent {
                                        set_icon_name: "system-search-symbolic",
                                        set_label: &tr!("Search for devices"),
                                    },
                                },
                            },
                            #[local_ref]
                            device_list -> gtk::ListBox {
                                set_selection_mode: gtk::SelectionMode::None,
                                set_valign: gtk::Align::Start,
                                add_css_class: "boxed-list",
                                add_css_class: "device-list",
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

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 14,
                        set_valign: gtk::Align::Fill,

                        gtk::Expander {
                            set_label: Some(&tr!("Help connecting a device")),
                            set_expanded: false,
                            #[wrap(Some)]
                            set_child = &gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 12,
                                set_margin_all: 12,
                                gtk::Label {
                                    set_label: &tr!("For Chromecast and DLNA, connect both devices to the same network."),
                                    set_xalign: 0.0, set_wrap: true,
                                },
                                gtk::Label {
                                    set_label: &tr!("For Miracast, open screen mirroring on the TV or projector, then refresh the device list."),
                                    set_xalign: 0.0, set_wrap: true,
                                },
                                gtk::Label {
                                    set_label: &tr!("You can also share with a web browser using the option below."),
                                    set_xalign: 0.0, set_wrap: true,
                                },
                            },
                        },
                    },
                },
                #[name = "web_card"]
                gtk::Box {
                    set_spacing: 20,
                    add_css_class: "info-card",
                    add_css_class: "ndi-card",
                    #[watch] set_visible: model.step == Step::Device,
                    gtk::Image { set_icon_name: Some("web-browser-symbolic"), set_pixel_size: 32, set_valign: gtk::Align::Start, add_css_class: "page-icon" },
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,
                        gtk::Label { set_label: &tr!("Share with a web browser"), set_xalign: 0.0, add_css_class: "title-3" },
                        gtk::Label { set_label: &tr!("Choose what to share. Then open the address and enter the PIN on a TV, phone or computer on the same network."), set_xalign: 0.0, set_wrap: true, set_max_width_chars: 80, add_css_class: "dim-label" },
                        #[name = "web_buttons"]
                        gtk::Box {
                            set_spacing: 12,
                            set_halign: gtk::Align::Start,
                            set_margin_top: 6,
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "video-display-symbolic", set_label: &tr!("Screen") },
                                connect_clicked => HomeMsg::PublishWeb(SourceType::Monitor),
                            },
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "window-new-symbolic", set_label: &tr!("Window") },
                                connect_clicked => HomeMsg::PublishWeb(SourceType::Window),
                            },
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "video-joined-displays-symbolic", set_label: &tr!("Extra screen") },
                                #[watch] set_sensitive: model.virtual_available,
                                connect_clicked => HomeMsg::PublishWeb(SourceType::Virtual),
                            },
                        },
                        gtk::Label {
                            set_label: &virtual_unavailable(),
                            #[watch] set_visible: !model.virtual_available,
                            set_xalign: 0.0,
                            set_wrap: true,
                            add_css_class: "caption",
                            add_css_class: "dim-label",
                        },
                    },
                },

                #[name = "ndi_card"]
                gtk::Box {
                    set_spacing: 20,
                    add_css_class: "info-card",
                    add_css_class: "ndi-card",
                    #[watch] set_visible: model.step == Step::Device,
                    gtk::Image { set_icon_name: Some("network-transmit-symbolic"), set_pixel_size: 32, set_valign: gtk::Align::Start, add_css_class: "page-icon" },
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,
                        gtk::Label { set_label: &tr!("Publish with NDI"), set_xalign: 0.0, add_css_class: "title-3" },
                        gtk::Label { set_label: &tr!("Choose this computer in OBS or another NDI receiver. Uses the name and audio options in Settings."), set_xalign: 0.0, set_wrap: true, set_max_width_chars: 80, add_css_class: "dim-label" },
                        #[name = "ndi_buttons"]
                        gtk::Box {
                            set_spacing: 12,
                            set_halign: gtk::Align::Start,
                            set_margin_top: 6,
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "video-display-symbolic", set_label: &tr!("Screen") },
                                connect_clicked => HomeMsg::PublishNdi(SourceType::Monitor),
                            },
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "window-new-symbolic", set_label: &tr!("Window") },
                                connect_clicked => HomeMsg::PublishNdi(SourceType::Window),
                            },
                            gtk::Button {
                                adw::ButtonContent { set_icon_name: "video-joined-displays-symbolic", set_label: &tr!("Extra screen") },
                                #[watch] set_sensitive: model.virtual_available,
                                connect_clicked => HomeMsg::PublishNdi(SourceType::Virtual),
                            },
                        },
                        gtk::Label {
                            set_label: &virtual_unavailable(),
                            #[watch] set_visible: !model.virtual_available,
                            set_xalign: 0.0,
                            set_wrap: true,
                            add_css_class: "caption",
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
        let mut first_tile = None;
        let mut virtual_button = None;
        let mut media_button = None;
        for (icon, tile, title, subtitle, message) in [
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
                tr!("Extra screen"),
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
            let button = action_button(icon, &title, &subtitle);
            let input = sender.input_sender().clone();
            let message = message.clone();
            button.connect_clicked(move |_| input.emit(message.clone()));
            if first_tile.is_none() {
                first_tile = Some(button.clone());
            }
            if tile == "virtual" {
                virtual_button = Some(button.clone());
            }
            if tile == "media" {
                media_button = Some(button.clone());
            }
            action_grid.append(&button);
        }

        let model = HomePage {
            devices,
            first_tile: first_tile.expect("the tiles are always built"),
            virtual_button: virtual_button.expect("the extra-screen button is always built"),
            media_button: media_button.expect("the media button is always built"),
            step: Step::Device,
            chosen: None,
            session: None,
            audio_summary: tr!("Audio"),
            virtual_available: false,
            qr_for: None,
        };
        model
            .devices
            .widget()
            .set_placeholder(Some(&crate::pages::discovery_placeholder(
                crate::pages::Discovery::Searching,
            )));

        let device_list = model.devices.widget();
        let action_grid = &action_grid;
        let widgets = view_output!();
        let compact = crate::pages::compact_breakpoint();
        let vertical = gtk::Orientation::Vertical.to_value();
        for stacked in [
            &widgets.receivers_heading,
            &widgets.ndi_card,
            &widgets.ndi_buttons,
            &widgets.web_card,
            &widgets.web_buttons,
        ] {
            compact.add_setter(stacked, "orientation", Some(&vertical));
        }
        big_gtk_kit::breakpoint::add(&root, compact);

        // Escape steps back from choosing what to send.
        let keys = gtk::EventControllerKey::new();
        let input = sender.input_sender().clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                input.emit(HomeMsg::Back);
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        root.add_controller(keys);

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        let step = self.step;
        let chosen = self.chosen.as_ref().map(|device| device.id.clone());
        match message {
            HomeMsg::AudioSummary(summary) => self.audio_summary = summary,
            HomeMsg::AudioSettings => {
                let _ = sender.output(HomeOutput::AudioSettings);
            }
            HomeMsg::Devices(entries) => {
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
            HomeMsg::Session(session) => {
                let url = session
                    .as_ref()
                    .and_then(|s| s.access.as_ref())
                    .map(|a| a.url.clone());
                if url != self.qr_for {
                    let texture = url.as_deref().and_then(super::qr::texture);
                    widgets.qr_picture.set_paintable(texture.as_ref());
                    self.qr_for = url;
                }
                self.session = session;
            }
            HomeMsg::Discovery(discovery) => {
                self.devices
                    .widget()
                    .set_placeholder(Some(&crate::pages::discovery_placeholder(discovery)));
            }
            HomeMsg::VirtualAvailable(available) => {
                self.virtual_available = available;
                // Left in place but insensitive: hiding it would leave the
                // person wondering whether the feature exists at all.
                self.virtual_button.set_sensitive(available);
                let reason = (!available).then(virtual_unavailable);
                self.virtual_button.set_tooltip_text(reason.as_deref());
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
            HomeMsg::PublishNdi(source) => {
                sender.output(HomeOutput::PublishNdi(source)).ok();
            }
            HomeMsg::PublishWeb(source) => {
                sender.output(HomeOutput::PublishWeb(source)).ok();
            }
            HomeMsg::CopyUrl => {
                if let Some(access) = self.access() {
                    widgets.qr_picture.clipboard().set_text(&access.url);
                }
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
            HomeMsg::Rescan => {
                if self.session.is_none() {
                    sender.output(HomeOutput::Rescan).ok();
                }
            }
            HomeMsg::Stop => {
                sender.output(HomeOutput::Stop).ok();
            }
        }
        self.media_button.set_visible(
            self.chosen
                .as_ref()
                .is_some_and(|device| device.kind.can_receive_files(&device.address)),
        );
        self.update_view(widgets, sender);
        // The control that had focus is hidden with its step; focus moves to
        // where the next choice is, or back to the receiver just left.
        if self.step != step {
            match self.step {
                Step::Action => {
                    self.first_tile.grab_focus();
                }
                Step::Device => {
                    if let Some(index) =
                        chosen.and_then(|id| self.devices.iter().position(|row| row.entry.id == id))
                        && let Some(row) = self.devices.widget().row_at_index(index as i32)
                    {
                        row.grab_focus();
                    }
                }
            }
        }
    }
}

impl HomePage {
    /// The heading, which follows the step.
    fn title(&self) -> String {
        match self.step {
            Step::Device => tr!("Where do you want to share?"),
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

    fn session_field<F: Fn(&SessionInfo) -> String>(&self, f: F) -> String {
        self.session.as_ref().map(f).unwrap_or_default()
    }

    fn access(&self) -> Option<&nd_core::sink::SinkAccess> {
        self.session.as_ref().and_then(|s| s.access.as_ref())
    }

    fn access_field<F: Fn(&nd_core::sink::SinkAccess) -> String>(&self, f: F) -> String {
        self.access().map(f).unwrap_or_default()
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
        if let Some(hint) = session.kind.and_then(latency_hint) {
            parts.push(hint);
        }
        parts.join(" · ")
    }
}

/// One thing that can be sent, as a tile button: pressing it starts sending.
fn action_button(icon: &str, title: &str, subtitle: &str) -> gtk::Button {
    let image = gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(32)
        .halign(gtk::Align::Start)
        .css_classes(["share-icon"])
        .build();

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    let name = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["heading"])
        .build();
    let detail = gtk::Label::builder()
        .label(subtitle)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(22)
        .css_classes(["dim-label", "caption"])
        .build();
    text.append(&name);
    text.append(&detail);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .build();
    let top = gtk::Box::builder().spacing(12).build();
    top.append(&image);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    top.append(&spacer);
    let arrow = gtk::Image::from_icon_name("go-next-symbolic");
    arrow.set_valign(gtk::Align::Center);
    arrow.add_css_class("tile-arrow");
    top.append(&arrow);
    content.append(&top);
    content.append(&text);

    gtk::Button::builder()
        .child(&content)
        .css_classes(["action-tile"])
        .build()
}

/// Why the extra-screen choices are greyed out.
fn virtual_unavailable() -> String {
    tr!("This desktop cannot create an extra screen.")
}
