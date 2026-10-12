//! Where to share: the running session, the receivers found, and the ways to
//! share that need no receiver of ours (web browsers, NDI).
//!
//! What is shared is chosen in the sidebar ([`super::options`]); every Share
//! button here starts that. For files, the list keeps the receivers that play
//! them, and the files to send are picked below it ([`super::media`]).

use relm4::adw::{self, prelude::*};
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;

use super::device_row::{DeviceRow, DeviceRowOutput, identity_refused, sync_rows};
use super::{DeviceEntry, Discovery, SessionInfo, latency_hint};
use crate::tr;

pub struct SharePage {
    devices: FactoryVecDeque<DeviceRow>,
    /// Every receiver found, before the files filter.
    all: Vec<DeviceEntry>,
    /// Files are shared instead of the screen.
    files: bool,
    /// Some file is chosen to send.
    chosen: bool,
    session: Option<SessionInfo>,
    /// Receivers whose remembered identity the person forgot. The service
    /// reports their refusal until the next attempt, which trusts whoever
    /// answers, so the row offers Share instead.
    retrusted: Vec<String>,
    /// The address the QR code on screen encodes, so the code is redrawn only
    /// when the address changes.
    qr_for: Option<String>,
}

#[derive(Debug)]
pub enum ShareMsg {
    Devices(Vec<DeviceEntry>),
    /// Whether files are shared, and whether any is chosen.
    Files {
        files: bool,
        chosen: bool,
    },
    Session(Option<SessionInfo>),
    Discovery(Discovery),
    Cast(String),
    /// Asks before forgetting the refused receiver's identity.
    TrustAgain(String),
    TrustConfirmed(String),
    Web,
    Ndi,
    /// Put the session's address on the clipboard (for OBS, a chat, a mail).
    CopyUrl,
    Stop,
}

#[derive(Debug)]
pub enum ShareOutput {
    Cast(String),
    Web,
    Ndi,
    Stop,
}

#[relm4::component(pub)]
impl Component for SharePage {
    type Init = ();
    type Input = ShareMsg;
    type Output = ShareOutput;
    /// The receiver whose identity was forgotten, and the outcome.
    type CommandOutput = (String, Result<(), String>);

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,

            adw::Clamp {
                set_maximum_size: 720,

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_all: 24,
                    set_spacing: 12,

                    // What is running, above the choices that would replace it.
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 6,
                        add_css_class: "hero-card",
                        #[watch]
                        set_visible: model.session.is_some(),

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
                            set_label: &model.connection_line(),
                            #[watch]
                            set_visible: !model.connection_line().is_empty(),
                            add_css_class: "hero-detail",
                            set_wrap: true,
                            set_justify: gtk::Justification::Center,
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
                                    connect_clicked => ShareMsg::CopyUrl,
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
                            connect_clicked => ShareMsg::Stop,
                        },
                    },

                    gtk::Label {
                        set_label: &tr!("Devices nearby"),
                        set_xalign: 0.0,
                        set_margin_top: 6,
                        add_css_class: "heading",
                    },
                    #[local_ref]
                    device_list -> gtk::ListBox {
                        set_selection_mode: gtk::SelectionMode::None,
                        add_css_class: "boxed-list",
                        add_css_class: "device-list",
                    },
                    gtk::Label {
                        #[watch]
                        set_label: &if model.files {
                            tr!("Only receivers that play files are listed: Chromecast, DLNA televisions and Miracast.")
                        } else {
                            tr!(
                                "Chromecast and DLNA receivers must be on the same network. For \
                                 Miracast, open screen mirroring on the TV, then search again."
                            )
                        },
                        set_xalign: 0.0,
                        set_wrap: true,
                        add_css_class: "caption",
                        add_css_class: "dim-label",
                    },

                    gtk::Label {
                        set_label: &tr!("Files"),
                        set_xalign: 0.0,
                        set_margin_top: 12,
                        add_css_class: "heading",
                        #[watch]
                        set_visible: model.files,
                    },
                    // The media section, put here by the window.
                    #[name = "files_slot"]
                    gtk::Box {
                        #[watch]
                        set_visible: model.files,
                    },

                    gtk::Label {
                        set_label: &tr!("Other ways to share"),
                        set_xalign: 0.0,
                        set_margin_top: 12,
                        add_css_class: "heading",
                        #[watch]
                        set_visible: !model.files,
                    },
                    gtk::ListBox {
                        #[watch]
                        set_visible: !model.files,
                        set_selection_mode: gtk::SelectionMode::None,
                        add_css_class: "boxed-list",
                        add_css_class: "device-list",

                        adw::ActionRow {
                            add_css_class: "device-row",
                            set_title: &tr!("Web browser"),
                            set_subtitle: &tr!("Any phone, tablet or computer on this network opens an address and enters a PIN"),
                            add_prefix = &gtk::Image {
                                set_icon_name: Some("web-browser-symbolic"),
                                set_pixel_size: 24,
                                add_css_class: "device-icon",
                            },
                            add_suffix = &gtk::Button {
                                set_valign: gtk::Align::Center,
                                gtk::Label { set_label: &tr!("Share") },
                                update_property: &[gtk::accessible::Property::Label(&tr!("Share with a web browser"))],
                                connect_clicked => ShareMsg::Web,
                            },
                        },
                        adw::ActionRow {
                            add_css_class: "device-row",
                            set_title: &tr!("NDI"),
                            set_subtitle: &tr!("For OBS and other video production software on the network"),
                            add_prefix = &gtk::Image {
                                set_icon_name: Some("network-transmit-symbolic"),
                                set_pixel_size: 24,
                                add_css_class: "device-icon",
                            },
                            add_suffix = &gtk::Button {
                                set_valign: gtk::Align::Center,
                                gtk::Label { set_label: &tr!("Share") },
                                update_property: &[gtk::accessible::Property::Label(&tr!("Publish with NDI"))],
                                connect_clicked => ShareMsg::Ndi,
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
        let devices: FactoryVecDeque<DeviceRow> = FactoryVecDeque::builder()
            .launch_default()
            .forward(sender.input_sender(), |output| match output {
                DeviceRowOutput::Cast(id) => ShareMsg::Cast(id),
                DeviceRowOutput::TrustAgain(id) => ShareMsg::TrustAgain(id),
            });
        devices
            .widget()
            .set_placeholder(Some(&crate::pages::discovery_placeholder(
                Discovery::Searching,
            )));
        let model = SharePage {
            devices,
            all: Vec::new(),
            files: false,
            chosen: false,
            session: None,
            retrusted: Vec::new(),
            qr_for: None,
        };
        let device_list = model.devices.widget();
        let widgets = view_output!();
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
            ShareMsg::Devices(entries) => {
                self.all = entries;
                self.show_devices();
            }
            ShareMsg::Files { files, chosen } => {
                if (files, chosen) != (self.files, self.chosen) {
                    self.files = files;
                    self.chosen = chosen;
                    self.show_devices();
                }
            }
            ShareMsg::Session(session) => {
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
            ShareMsg::Discovery(discovery) => {
                self.devices
                    .widget()
                    .set_placeholder(Some(&crate::pages::discovery_placeholder(discovery)));
            }
            ShareMsg::Cast(id) => {
                sender.output(ShareOutput::Cast(id)).ok();
            }
            ShareMsg::Web => {
                sender.output(ShareOutput::Web).ok();
            }
            ShareMsg::Ndi => {
                sender.output(ShareOutput::Ndi).ok();
            }
            ShareMsg::CopyUrl => {
                if let Some(access) = self.access() {
                    widgets.qr_picture.clipboard().set_text(&access.url);
                }
            }
            ShareMsg::Stop => {
                sender.output(ShareOutput::Stop).ok();
            }
            ShareMsg::TrustAgain(id) => {
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Trust this receiver again?")),
                    Some(&tr!(
                        "The next device that answers for this receiver will be trusted and \
                         remembered. Do this only if you replaced or reset it and nobody else \
                         can reach this network."
                    )),
                );
                dialog.add_response("cancel", &tr!("Cancel"));
                dialog.add_response("trust", &tr!("Trust again"));
                dialog.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let input = sender.input_sender().clone();
                dialog.connect_response(Some("trust"), move |_, _| {
                    input.emit(ShareMsg::TrustConfirmed(id.clone()))
                });
                dialog.present(Some(root));
            }
            ShareMsg::TrustConfirmed(id) => {
                sender.oneshot_command(async move {
                    let forget = id.clone();
                    let result =
                        relm4::spawn_blocking(move || nd_chromecast::identity::forget(&forget))
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(|result| result.map_err(|err| err.to_string()));
                    (id, result)
                });
            }
        }
        self.update_view(widgets, sender);
    }

    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        (id, outcome): Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match outcome {
            Ok(()) => {
                self.retrusted.push(id.clone());
                self.show_devices();
                root.announce(
                    &tr!("The device that answers on the next connection will be trusted"),
                    gtk::AccessibleAnnouncementPriority::Medium,
                );
            }
            Err(err) => {
                tracing::warn!(%err, "could not forget the receiver's identity");
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Could not forget this receiver's identity")),
                    Some(&err),
                );
                dialog.add_response("close", &tr!("Close"));
                dialog.present(Some(root));
            }
        }
        self.update_view(widgets, sender);
    }
}

impl SharePage {
    /// The rows for the receivers found, as the current mode offers them.
    fn show_devices(&mut self) {
        // A refusal recorded before the identity was forgotten is shown as a
        // receiver ready to try again.
        let all = &self.all;
        self.retrusted
            .retain(|id| all.iter().any(|e| &e.id == id && identity_refused(e)));
        let blocked = (self.files && !self.chosen).then(|| tr!("Select files below first"));
        let entries = self
            .all
            .iter()
            .filter(|entry| !self.files || entry.kind.can_receive_files(&entry.address))
            .map(|entry| {
                let mut entry = entry.clone();
                if self.retrusted.contains(&entry.id) {
                    entry.state = nd_core::sink::SinkState::Disconnected;
                    entry.detail.clear();
                }
                entry.blocked = blocked.clone();
                entry
            })
            .collect();
        sync_rows(&mut self.devices, entries);
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

    /// Address and protocol, then what is known about the link, measured
    /// rather than assumed.
    fn connection_line(&self) -> String {
        let Some(session) = &self.session else {
            return String::new();
        };
        let mut parts = Vec::new();
        if !session.address.is_empty() {
            parts.push(session.address.clone());
        }
        parts.push(session.protocol.clone());
        if session.measurable {
            parts.push(super::quality_line(session));
        }
        if let Some(hint) = session.kind.and_then(latency_hint) {
            parts.push(hint);
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nd_core::sink::{SinkKind, SinkState};

    #[test]
    #[ignore = "requires a graphical GTK session and a temporary XDG_DATA_HOME"]
    fn retrust_forgets_only_identity() {
        let data_home = std::env::var_os("XDG_DATA_HOME").expect("XDG_DATA_HOME");
        assert!(
            std::path::Path::new(&data_home).starts_with(std::env::temp_dir()),
            "run with a temporary XDG_DATA_HOME; this test forgets identities"
        );
        adw::init().expect("GTK session");
        let flush = || {
            gtk::glib::MainContext::default().block_on(gtk::glib::timeout_future(
                std::time::Duration::from_millis(50),
            ));
        };
        let receiver = "Test-TV._googlecast._tcp.local.";
        nd_chromecast::identity::check_and_remember(receiver, Some(b"old-device")).unwrap();
        let page = SharePage::builder().launch(()).detach();
        let mut refused = DeviceEntry {
            id: receiver.into(),
            name: "Receiver".into(),
            protocol: super::super::protocol_label(SinkKind::Chromecast),
            address: "10.0.0.2".into(),
            kind: SinkKind::Chromecast,
            castable: true,
            state: SinkState::Error,
            detail: format!(
                "protocol: changed: {}",
                nd_chromecast::identity::IDENTITY_CHANGED
            ),
            mode: String::new(),
            blocked: None,
        };
        page.emit(ShareMsg::Devices(vec![refused.clone()]));
        flush();
        let shown = |page: &relm4::Controller<SharePage>| {
            identity_refused(&page.model().devices.iter().next().unwrap().entry)
        };
        assert!(shown(&page));

        page.emit(ShareMsg::TrustConfirmed(receiver.into()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while page.model().retrusted.is_empty() && std::time::Instant::now() < deadline {
            gtk::glib::MainContext::default().iteration(false);
        }
        flush();
        assert!(!shown(&page), "the refusal stays after forgetting");
        // Forgotten: a different device is a first use, and is remembered.
        nd_chromecast::identity::check_and_remember(receiver, Some(b"new-device")).unwrap();
        nd_chromecast::identity::check_and_remember(receiver, Some(b"old-device")).unwrap_err();

        // The next attempt replaces the note.
        refused.state = SinkState::Connecting;
        page.emit(ShareMsg::Devices(vec![refused]));
        flush();
        assert!(page.model().retrusted.is_empty());
    }
}
