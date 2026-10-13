//! The settings that are rarely changed, shown in a dialog, and the owner of
//! every setting: the sidebar's choices ([`super::options`]) arrive here as
//! [`SettingsMsg::Replace`] and are saved with the rest.
//!
//! Every control here is wired to [`nd_core::settings`], and every field of
//! that struct is read by the code that builds a session. Encryption is not a
//! choice: Cast mirroring fixes its own cipher and Miracast offers none.
//!
//! Changes take effect on the next session. The resolution and the audio
//! branch are fixed when the pipeline is built, and rebuilding it under a live
//! cast would drop the picture on the wall.

use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_core::settings::{self, Protocol, Settings};

use crate::tr;

/// How long the controls have to settle before the file is written.
///
/// A slider emits dozens of values per second, and a file write for each is
/// wasted work. In memory the change is immediate, only the disk waits.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

pub struct SettingsPage {
    settings: Settings,
    /// The write scheduled for when the controls settle, if any.
    pending_write: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
}

#[derive(Debug)]
pub enum SettingsMsg {
    /// Settings changed in the sidebar.
    Replace(Settings),
    SetProtocol(Protocol),
    SetHardwareEncoding(bool),
    SetAutoDiscovery(bool),
    SetDeviceName(String),
    SetPort(u16),
    Reset,
    ResetConfirmed,
}

#[derive(Debug)]
pub enum SettingsOutput {
    /// The settings were changed; the root component may need to act on it.
    Changed(Settings),
    Saved(Result<(), String>),
}

#[relm4::component(pub)]
impl Component for SettingsPage {
    type Init = ();
    type Input = SettingsMsg;
    type Output = SettingsOutput;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_propagate_natural_height: true,

            adw::Clamp {
                set_maximum_size: 760,

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_all: 18,
                    add_css_class: "settings-page",
                    set_spacing: 6,

                    adw::PreferencesGroup {
                        set_title: &tr!("Discovery and devices"),

                        #[name = "protocol"]
                        adw::ComboRow {
                            set_title: &tr!("Preferred protocol"),
                            add_prefix = &gtk::Image { set_icon_name: Some("video-display-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_use_subtitle: true,
                            set_tooltip_text: Some(&tr!("Which kind of receiver to look for")),
                            set_model: Some(&string_list(&[
                                tr!("Automatic"),
                                tr!("Miracast only"),
                                tr!("Local network only (Chromecast, DLNA, AirPlay)"),
                            ])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetProtocol(match row.selected() {
                                    1 => Protocol::Miracast,
                                    2 => Protocol::Cast,
                                    _ => Protocol::Auto,
                                }));
                            } @protocol_handler,
                        },
                        #[name = "auto_discovery"]
                        adw::SwitchRow {
                            set_title: &tr!("Automatic discovery"),
                            add_prefix = &gtk::Image { set_icon_name: Some("network-wireless-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_subtitle: &tr!("Look for receivers as soon as the app starts"),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetAutoDiscovery(row.is_active()));
                            } @auto_discovery_handler,
                        },

                        #[name = "device_name"]
                        adw::EntryRow {
                            set_title: &tr!("This computer's name"),
                            add_prefix = &gtk::Image { set_icon_name: Some("computer-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            connect_changed[sender] => move |row| {
                                sender.input(SettingsMsg::SetDeviceName(row.text().to_string()));
                            } @device_name_handler,
                        },
                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Advanced"),
                        set_margin_top: 18,
                        #[name = "hardware_encoding"]
                        adw::SwitchRow {
                            set_title: &tr!("Hardware acceleration"),
                            add_prefix = &gtk::Image { set_icon_name: Some("applications-graphics-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_subtitle: &tr!(
                                "Encode on the graphics card, which is faster and uses far \
                                 less processor. Turn it off if the picture arrives wrong, \
                                 broken or not at all."
                            ),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetHardwareEncoding(row.is_active()));
                            } @hardware_encoding_handler,
                        },
                        adw::ActionRow {
                            set_title: &tr!("Fixed port"),
                            add_prefix = &gtk::Image { set_icon_name: Some("network-wired-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_subtitle: &tr!(
                                "0 lets the system choose. Set one only if you opened a \
                                 port on your firewall by hand."
                            ),

                            #[name = "port"]
                            add_suffix = &gtk::SpinButton {
                                update_property: &[gtk::accessible::Property::Label(&tr!("Fixed port"))],
                                set_valign: gtk::Align::Center,
                                set_adjustment: &gtk::Adjustment::new(
                                    0.0, 0.0, 65535.0, 1.0, 100.0, 0.0,
                                ),
                                set_numeric: true,
                                connect_value_changed[sender] => move |spin| {
                                    sender.input(SettingsMsg::SetPort(spin.value() as u16));
                                } @port_handler,
                            },
                        },
                    },

                    gtk::Box {
                        set_spacing: 12,
                        set_margin_top: 18,
                        add_css_class: "info-card",

                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_hexpand: true,

                            gtk::Label {
                                set_label: &tr!("Changes are saved automatically"),
                                set_xalign: 0.0,
                                add_css_class: "heading",
                            },
                        },
                        gtk::Button {
                            set_valign: gtk::Align::Center,
                            connect_clicked => SettingsMsg::Reset,
                            adw::ButtonContent {
                                set_icon_name: "view-refresh-symbolic",
                                set_label: &tr!("Restore defaults"),
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
        let model = SettingsPage {
            settings: settings::current(),
            pending_write: Default::default(),
        };
        let widgets = view_output!();
        model.show(&widgets);
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
            SettingsMsg::Replace(settings) => {
                self.settings = settings;
                self.show(widgets);
            }
            SettingsMsg::SetProtocol(protocol) => self.settings.protocol = protocol,
            SettingsMsg::SetHardwareEncoding(on) => self.settings.hardware_encoding = on,
            SettingsMsg::SetAutoDiscovery(on) => self.settings.auto_discovery = on,
            SettingsMsg::SetDeviceName(name) => self.settings.device_name = name,
            SettingsMsg::SetPort(port) => self.settings.port = port,
            SettingsMsg::Reset => {
                let dialog = adw::AlertDialog::new(
                    Some(&tr!("Restore default settings?")),
                    Some(&tr!(
                        "Every setting returns to its default, including this computer's name and the fixed port."
                    )),
                );
                dialog.add_response("cancel", &tr!("Cancel"));
                dialog.add_response("restore", &tr!("Restore"));
                dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let input = sender.input_sender().clone();
                dialog.connect_response(Some("restore"), move |_, _| {
                    input.emit(SettingsMsg::ResetConfirmed)
                });
                dialog.present(Some(root));
                return;
            }
            SettingsMsg::ResetConfirmed => {
                self.settings = Settings::default();
                self.show(widgets);
            }
        }

        self.schedule_write(sender.output_sender().clone());
        sender
            .output(SettingsOutput::Changed(self.settings.clone()))
            .ok();
        self.update_view(widgets, sender);
    }

    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        // The window flushes asynchronously before allowing shutdown.
        if let Some(pending) = self.pending_write.take() {
            big_gtk_kit::source::remove(pending);
        }
    }
}

impl SettingsPage {
    /// Applies the settings now and saves them once the controls settle,
    /// off the GTK thread (see [`SAVE_DELAY`]).
    fn schedule_write(&mut self, output: relm4::Sender<SettingsOutput>) {
        settings::set_in_memory(&self.settings);
        if let Some(pending) = self.pending_write.take() {
            big_gtk_kit::source::remove(pending);
        }
        let slot = self.pending_write.clone();
        let id = gtk::glib::timeout_add_local_once(SAVE_DELAY, move || {
            // The source is gone once it has fired; forget its id so a later
            // cancellation does not try to remove it twice.
            slot.set(None);
            relm4::spawn(async move {
                let result = relm4::spawn_blocking(settings::persist)
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|result| result.map_err(|err| err.to_string()));
                let _ = output.send(SettingsOutput::Saved(result));
            });
        });
        self.pending_write.set(Some(id));
    }

    /// Writes the model into the controls with their handlers blocked, so the
    /// values set here do not read back as the person changing them.
    fn show(&self, widgets: &SettingsPageWidgets) {
        let w = widgets;
        let handlers: [(&gtk::glib::Object, &gtk::glib::SignalHandlerId); 5] = [
            (w.protocol.upcast_ref(), &w.protocol_handler),
            (
                w.hardware_encoding.upcast_ref(),
                &w.hardware_encoding_handler,
            ),
            (w.auto_discovery.upcast_ref(), &w.auto_discovery_handler),
            (w.device_name.upcast_ref(), &w.device_name_handler),
            (w.port.upcast_ref(), &w.port_handler),
        ];
        for (object, handler) in handlers {
            object.block_signal(handler);
        }

        widgets.protocol.set_selected(match self.settings.protocol {
            Protocol::Auto => 0,
            Protocol::Miracast => 1,
            Protocol::Cast => 2,
        });
        widgets
            .hardware_encoding
            .set_active(self.settings.hardware_encoding);
        widgets
            .auto_discovery
            .set_active(self.settings.auto_discovery);
        widgets.device_name.set_text(&self.settings.device_name);
        widgets.port.set_value(self.settings.port as f64);

        for (object, handler) in handlers {
            object.unblock_signal(handler);
        }
    }
}

fn string_list(items: &[String]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for item in items {
        list.append(item);
    }
    list
}
