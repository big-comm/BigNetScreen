//! Every receiver found, with what is known about the one selected.
//!
//! The home page lists receivers so that one can be picked; this page is for
//! looking at them — filtering by protocol, rescanning, and reading what the
//! discovery actually told us about a device before sending a screen to it.
//!
//! What it does **not** show is as deliberate as what it does. There is no
//! firmware version, no MAC address for a Chromecast and no "last seen": mDNS
//! and Wi-Fi Direct do not report those, and a panel of plausible-looking
//! fields invented to fill space is worse than a short one that is true.

use relm4::adw::{self, prelude::*};
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;

use nd_core::settings;
use nd_core::sink::SinkState;

use super::home::{DeviceRow, DeviceRowOutput};
use super::{latency_hint, state_label, DeviceEntry};
use crate::tr;

/// Which protocols the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Cast,
    Miracast,
}

impl Filter {
    fn matches(self, entry: &DeviceEntry) -> bool {
        use nd_core::sink::SinkKind;
        match self {
            Filter::All => true,
            Filter::Cast => matches!(entry.kind, SinkKind::Chromecast | SinkKind::AirPlay),
            Filter::Miracast => matches!(entry.kind, SinkKind::WfdP2p | SinkKind::WfdMice),
        }
    }
}

pub struct DevicesPage {
    devices: FactoryVecDeque<DeviceRow>,
    /// Everything found, before filtering.
    all: Vec<DeviceEntry>,
    filter: Filter,
    selected: Option<DeviceEntry>,
    auto_discovery: bool,
    /// The id of the receiver being streamed to.
    active: Option<String>,
}

#[derive(Debug)]
pub enum DevicesMsg {
    Devices(Vec<DeviceEntry>),
    Active(Option<String>),
    Selected(String),
    SetFilter(Filter),
    SetAutoDiscovery(bool),
    Rescan,
    Cast,
    Stop,
    SendMedia,
}

#[derive(Debug)]
pub enum DevicesOutput {
    Cast(String),
    Stop,
    Rescan,
    AutoDiscovery(bool),
    /// Go to the media page with this receiver in mind.
    SendMedia(String),
}

#[relm4::component(pub)]
impl Component for DevicesPage {
    type Init = ();
    type Input = DevicesMsg;
    type Output = DevicesOutput;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_margin_all: 24,
                set_spacing: 6,

                gtk::Box {
                    set_spacing: 12,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_hexpand: true,

                        gtk::Label {
                            set_label: &tr!("Devices"),
                            set_xalign: 0.0,
                            add_css_class: "page-title",
                        },
                        gtk::Label {
                            set_label: &tr!("Find and connect to nearby wireless receivers."),
                            set_xalign: 0.0,
                            add_css_class: "page-subtitle",
                        },
                    },

                    gtk::Button {
                        set_valign: gtk::Align::Center,
                        set_tooltip_text: Some(&tr!("Scan again")),
                        connect_clicked => DevicesMsg::Rescan,

                        adw::ButtonContent {
                            set_icon_name: "view-refresh-symbolic",
                            set_label: &tr!("Refresh"),
                        },
                    },

                    #[name = "filter_group"]
                    adw::ToggleGroup {
                        set_valign: gtk::Align::Center,
                        // The toggles are added in `init`: the builder takes
                        // them by value, which the view macro cannot express.
                        connect_active_notify[sender] => move |group| {
                            sender.input(DevicesMsg::SetFilter(match group.active() {
                                1 => Filter::Cast,
                                2 => Filter::Miracast,
                                _ => Filter::All,
                            }));
                        },
                    },
                },

                gtk::Box {
                    set_spacing: 18,
                    set_margin_top: 18,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_hexpand: true,

                        #[local_ref]
                        device_list -> gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            set_valign: gtk::Align::Start,
                            add_css_class: "boxed-list",
                        },

                        gtk::Box {
                            set_spacing: 8,
                            set_margin_top: 6,

                            gtk::Label {
                                #[watch]
                                set_label: &found_summary(model.all.len()),
                                set_hexpand: true,
                                set_xalign: 0.0,
                                add_css_class: "dim-label",
                            },

                            gtk::Label {
                                set_label: &tr!("Automatic discovery"),
                                add_css_class: "dim-label",
                            },
                            #[name = "auto_switch"]
                            gtk::Switch {
                                set_valign: gtk::Align::Center,
                                connect_state_set[sender] => move |_, state| {
                                    sender.input(DevicesMsg::SetAutoDiscovery(state));
                                    gtk::glib::Propagation::Proceed
                                },
                            },
                        },
                    },

                    // The detail panel. Empty until a row is picked, because
                    // there is nothing truthful to put in it before that.
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 12,
                        set_width_request: 300,
                        set_valign: gtk::Align::Start,
                        add_css_class: "info-card",
                        #[watch]
                        set_visible: model.selected.is_some(),

                        gtk::Label {
                            #[watch]
                            set_label: &model.field(|d| d.name.clone()),
                            set_xalign: 0.0,
                            set_wrap: true,
                            add_css_class: "title-2",
                        },
                        gtk::Label {
                            #[watch]
                            set_label: &model.field(|d| d.subtitle()),
                            set_xalign: 0.0,
                            set_wrap: true,
                            add_css_class: "dim-label",
                        },

                        gtk::Box {
                            set_spacing: 8,
                            set_margin_top: 6,
                            set_homogeneous: true,

                            gtk::Button {
                                #[watch]
                                set_visible: !model.is_selected_active(),
                                #[watch]
                                set_sensitive: model.field_bool(|d| d.castable),
                                add_css_class: "suggested-action",
                                connect_clicked => DevicesMsg::Cast,

                                adw::ButtonContent {
                                    set_icon_name: "video-display-symbolic",
                                    set_label: &tr!("Mirror screen"),
                                },
                            },
                            gtk::Button {
                                #[watch]
                                set_visible: model.is_selected_active(),
                                add_css_class: "destructive-action",
                                connect_clicked => DevicesMsg::Stop,

                                adw::ButtonContent {
                                    set_icon_name: "media-playback-stop-symbolic",
                                    set_label: &tr!("Disconnect"),
                                },
                            },
                            gtk::Button {
                                // Only a Chromecast plays a file we send; the
                                // rest of the protocols have no such notion.
                                #[watch]
                                set_visible: model.field_bool(|d| {
                                    d.kind == nd_core::sink::SinkKind::Chromecast
                                }),
                                connect_clicked => DevicesMsg::SendMedia,

                                adw::ButtonContent {
                                    set_icon_name: "folder-videos-symbolic",
                                    set_label: &tr!("Send media"),
                                },
                            },
                        },

                        gtk::ListBox {
                            set_selection_mode: gtk::SelectionMode::None,
                            set_margin_top: 6,
                            add_css_class: "boxed-list",

                            adw::ActionRow {
                                set_title: &tr!("Protocol"),
                                #[watch]
                                set_subtitle: &model.field(|d| d.protocol.clone()),
                            },
                            adw::ActionRow {
                                set_title: &tr!("Address"),
                                #[watch]
                                set_subtitle: &model.field(|d| {
                                    if d.address.is_empty() {
                                        tr!("not published yet")
                                    } else {
                                        d.address.clone()
                                    }
                                }),
                            },
                            adw::ActionRow {
                                set_title: &tr!("Status"),
                                #[watch]
                                set_subtitle: &model.status_line(),
                            },
                            adw::ActionRow {
                                set_title: &tr!("Streaming at"),
                                #[watch]
                                set_subtitle: &model.field(|d| d.mode.clone()),
                                // Only while there is a session: this is what
                                // the two ends agreed on, not what the receiver
                                // is capable of.
                                #[watch]
                                set_visible: !model.field(|d| d.mode.clone()).is_empty(),
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
                    DeviceRowOutput::Activated(id) => DevicesMsg::Selected(id),
                });

        let model = DevicesPage {
            devices,
            all: Vec::new(),
            filter: Filter::All,
            selected: None,
            auto_discovery: settings::current().auto_discovery,
            active: None,
        };

        let device_list = model.devices.widget();
        let widgets = view_output!();

        for label in [
            tr!("All protocols"),
            tr!("Chromecast / AirPlay"),
            tr!("Miracast"),
        ] {
            widgets
                .filter_group
                .add(adw::Toggle::builder().label(&label).build());
        }
        widgets.filter_group.set_active(0);
        widgets.auto_switch.set_active(model.auto_discovery);

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
            DevicesMsg::Devices(entries) => {
                // Keep the panel in step with the list: a receiver that starts
                // streaming, or drops off the network, must not leave a stale
                // description of itself on screen.
                if let Some(selected) = &self.selected {
                    self.selected = entries.iter().find(|e| e.id == selected.id).cloned();
                }
                self.all = entries;
                self.apply_filter();
            }
            DevicesMsg::Active(active) => self.active = active,
            DevicesMsg::Selected(id) => {
                self.selected = self.all.iter().find(|e| e.id == id).cloned();
            }
            DevicesMsg::SetFilter(filter) => {
                self.filter = filter;
                self.apply_filter();
            }
            DevicesMsg::SetAutoDiscovery(on) => {
                self.auto_discovery = on;
                sender.output(DevicesOutput::AutoDiscovery(on)).ok();
            }
            DevicesMsg::Rescan => {
                sender.output(DevicesOutput::Rescan).ok();
            }
            DevicesMsg::Cast => {
                if let Some(selected) = &self.selected {
                    sender.output(DevicesOutput::Cast(selected.id.clone())).ok();
                }
            }
            DevicesMsg::Stop => {
                sender.output(DevicesOutput::Stop).ok();
            }
            DevicesMsg::SendMedia => {
                if let Some(selected) = &self.selected {
                    sender
                        .output(DevicesOutput::SendMedia(selected.id.clone()))
                        .ok();
                }
            }
        }
        self.update_view(widgets, sender);
    }
}

impl DevicesPage {
    fn apply_filter(&mut self) {
        let visible: Vec<DeviceEntry> = self
            .all
            .iter()
            .filter(|entry| self.filter.matches(entry))
            .cloned()
            .collect();
        super::home::sync_rows(&mut self.devices, visible);
    }

    fn field<F: Fn(&DeviceEntry) -> String>(&self, f: F) -> String {
        self.selected.as_ref().map(f).unwrap_or_default()
    }

    fn field_bool<F: Fn(&DeviceEntry) -> bool>(&self, f: F) -> bool {
        self.selected.as_ref().map(f).unwrap_or(false)
    }

    fn is_selected_active(&self) -> bool {
        match (&self.selected, &self.active) {
            (Some(selected), Some(active)) => &selected.id == active,
            _ => false,
        }
    }

    /// What the receiver is doing, and what to expect of it.
    fn status_line(&self) -> String {
        let Some(selected) = &self.selected else {
            return String::new();
        };
        if selected.state == SinkState::Error && !selected.detail.is_empty() {
            return selected.detail.clone();
        }
        let state = match selected.state {
            SinkState::Disconnected if selected.castable => tr!("Ready"),
            SinkState::Disconnected => tr!("Discovery only"),
            state => state_label(state),
        };
        match latency_hint(selected.kind).filter(|_| selected.castable) {
            Some(hint) => format!("{state} · {hint}"),
            None => state,
        }
    }
}

fn found_summary(count: usize) -> String {
    match count {
        0 => tr!("No devices found"),
        1 => tr!("1 device found"),
        n => format!("{n} {}", tr!("devices found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nd_core::sink::SinkKind;

    fn entry(kind: SinkKind) -> DeviceEntry {
        DeviceEntry {
            id: format!("{kind:?}"),
            name: "Receiver".into(),
            protocol: super::super::protocol_label(kind),
            address: "10.0.0.2".into(),
            kind,
            castable: kind.is_castable(),
            state: SinkState::Disconnected,
            detail: String::new(),
            mode: String::new(),
        }
    }

    #[test]
    fn the_filter_keeps_related_protocols_together() {
        // "Cast" is the network family: a person filtering for it is looking
        // for the box on the TV, and does not care that one speaks Google's
        // protocol and the other Apple's.
        assert!(Filter::Cast.matches(&entry(SinkKind::Chromecast)));
        assert!(Filter::Cast.matches(&entry(SinkKind::AirPlay)));
        assert!(!Filter::Cast.matches(&entry(SinkKind::WfdP2p)));

        assert!(Filter::Miracast.matches(&entry(SinkKind::WfdP2p)));
        assert!(Filter::Miracast.matches(&entry(SinkKind::WfdMice)));
        assert!(!Filter::Miracast.matches(&entry(SinkKind::Chromecast)));
    }

    #[test]
    fn the_default_filter_hides_nothing() {
        for kind in [
            SinkKind::Chromecast,
            SinkKind::AirPlay,
            SinkKind::WfdP2p,
            SinkKind::WfdMice,
            SinkKind::Dummy,
        ] {
            assert!(
                Filter::default().matches(&entry(kind)),
                "{kind:?} disappeared with no filter set"
            );
        }
    }
}
