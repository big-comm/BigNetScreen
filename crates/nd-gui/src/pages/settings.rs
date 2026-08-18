//! The preferences, and only the ones that change something.
//!
//! Every control here is wired to [`nd_core::settings`], and every field of
//! that struct is read by the code that builds a session. Some rows from the
//! original design are deliberately absent — end-to-end encryption cannot be
//! chosen (Cast mirroring fixes its own cipher and Miracast offers none), and
//! subtitle handling belongs to a media player this application does not have.
//! A switch that does nothing is a lie told in a nice font.
//!
//! Changes take effect on the **next** session, not the running one. The
//! resolution and the audio branch are fixed when the pipeline is built, and
//! rebuilding it under a live cast would drop the picture on the wall.

use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_core::settings::{self, Protocol, Quality, Settings};

use crate::tr;

pub struct SettingsPage {
    settings: Settings,
    /// Set while the widgets are being brought in line with the model, so that
    /// setting a control's value does not read back as the person changing it.
    loading: bool,
}

#[derive(Debug)]
pub enum SettingsMsg {
    SetProtocol(Protocol),
    SetQuality(Quality),
    SetFps(u32),
    SetSystemAudio(bool),
    SetMicrophone(bool),
    SetMicVolume(f64),
    SetAutoDiscovery(bool),
    SetFilmMode(bool),
    SetDeviceName(String),
    SetPort(u16),
    Reset,
    /// The settings changed somewhere else (the devices page has the discovery
    /// switch too).
    Reload,
}

#[derive(Debug)]
pub enum SettingsOutput {
    /// The settings were changed; the root component may need to act on it.
    Changed(Settings),
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

            adw::Clamp {
                set_maximum_size: 760,

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_all: 24,
                    set_spacing: 6,

                    gtk::Label {
                        set_label: &tr!("Settings"),
                        set_xalign: 0.0,
                        add_css_class: "page-title",
                    },
                    gtk::Label {
                        set_label: &tr!("These apply to your next session."),
                        set_xalign: 0.0,
                        set_margin_bottom: 12,
                        add_css_class: "page-subtitle",
                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Streaming"),
                        set_margin_top: 12,

                        #[name = "protocol"]
                        adw::ComboRow {
                            set_title: &tr!("Preferred protocol"),
                            set_subtitle: &tr!("Which kind of receiver to look for"),
                            set_model: Some(&string_list(&[
                                tr!("Automatic"),
                                tr!("Miracast only"),
                                tr!("Chromecast / AirPlay only"),
                            ])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetProtocol(match row.selected() {
                                    1 => Protocol::Miracast,
                                    2 => Protocol::Cast,
                                    _ => Protocol::Auto,
                                }));
                            },
                        },

                        #[name = "quality"]
                        adw::ComboRow {
                            set_title: &tr!("Streaming quality"),
                            set_subtitle: &tr!(
                                "A ceiling: a smaller screen is sent as it is. Above 1080p \
                                 not every receiver will accept it."
                            ),
                            set_model: Some(&string_list(&[
                                tr!("Maximum (2160p)"),
                                tr!("Very high (1440p)"),
                                tr!("High (1080p)"),
                                tr!("Medium (720p)"),
                                tr!("Low (480p)"),
                            ])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetQuality(match row.selected() {
                                    0 => Quality::Max,
                                    1 => Quality::Ultra,
                                    3 => Quality::Medium,
                                    4 => Quality::Low,
                                    _ => Quality::High,
                                }));
                            },
                        },

                        #[name = "fps"]
                        adw::ComboRow {
                            set_title: &tr!("Frame rate"),
                            set_subtitle: &tr!(
                                "60 is smoother and costs about half as much again in bandwidth"
                            ),
                            set_model: Some(&string_list(&[tr!("30 FPS"), tr!("60 FPS")])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetFps(
                                    if row.selected() == 1 { 60 } else { 30 },
                                ));
                            },
                        },

                        #[name = "film_mode"]
                        adw::SwitchRow {
                            set_title: &tr!("Film mode"),
                            set_subtitle: &tr!(
                                "Buffers the picture so video plays smoothly, at the cost of \
                                 delay. On Miracast it is applied here; on Chromecast it is a \
                                 request the receiver may ignore."
                            ),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetFilmMode(row.is_active()));
                            },
                        },
                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Audio"),
                        set_margin_top: 18,

                        #[name = "system_audio"]
                        adw::SwitchRow {
                            set_title: &tr!("Include system audio"),
                            set_subtitle: &tr!("Send whatever this computer is playing"),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetSystemAudio(row.is_active()));
                            },
                        },

                        #[name = "microphone"]
                        adw::SwitchRow {
                            set_title: &tr!("Include the microphone"),
                            set_subtitle: &tr!("Mix your voice into what is sent"),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetMicrophone(row.is_active()));
                            },
                        },

                        adw::ActionRow {
                            set_title: &tr!("Microphone volume"),
                            #[watch]
                            set_sensitive: model.settings.microphone,

                            #[name = "mic_volume"]
                            add_suffix = &gtk::Scale {
                                set_valign: gtk::Align::Center,
                                set_width_request: 220,
                                set_draw_value: true,
                                set_value_pos: gtk::PositionType::Right,
                                set_adjustment: &gtk::Adjustment::new(
                                    80.0, 0.0, 100.0, 5.0, 10.0, 0.0,
                                ),
                                set_digits: 0,
                                connect_value_changed[sender] => move |scale| {
                                    sender.input(SettingsMsg::SetMicVolume(scale.value()));
                                },
                            },
                        },
                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Discovery and devices"),
                        set_margin_top: 18,

                        #[name = "auto_discovery"]
                        adw::SwitchRow {
                            set_title: &tr!("Automatic discovery"),
                            set_subtitle: &tr!("Look for receivers as soon as the app starts"),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetAutoDiscovery(row.is_active()));
                            },
                        },

                        #[name = "device_name"]
                        adw::EntryRow {
                            set_title: &tr!("This computer's name"),
                            connect_changed[sender] => move |row| {
                                sender.input(SettingsMsg::SetDeviceName(row.text().to_string()));
                            },
                        },

                        adw::ActionRow {
                            set_title: &tr!("Fixed port"),
                            set_subtitle: &tr!(
                                "0 lets the system choose. Set one only if you opened a \
                                 port on your firewall by hand."
                            ),

                            #[name = "port"]
                            add_suffix = &gtk::SpinButton {
                                set_valign: gtk::Align::Center,
                                set_adjustment: &gtk::Adjustment::new(
                                    0.0, 0.0, 65535.0, 1.0, 100.0, 0.0,
                                ),
                                set_numeric: true,
                                connect_value_changed[sender] => move |spin| {
                                    sender.input(SettingsMsg::SetPort(spin.value() as u16));
                                },
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
                                set_label: &tr!("Saved as you change them"),
                                set_xalign: 0.0,
                                add_css_class: "heading",
                            },
                            gtk::Label {
                                #[watch]
                                set_label: &settings_path_line(),
                                set_xalign: 0.0,
                                set_wrap: true,
                                add_css_class: "dim-label",
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
            loading: true,
        };
        let widgets = view_output!();
        let mut model = model;
        model.show(&widgets);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        // While the widgets are being filled in from the model, their "changed"
        // signals are echoes of what was just written, not the person choosing
        // something. Acting on them would save the file on every reload.
        if self.loading && !matches!(message, SettingsMsg::Reload) {
            return;
        }

        match message {
            SettingsMsg::SetProtocol(protocol) => self.settings.protocol = protocol,
            SettingsMsg::SetQuality(quality) => self.settings.quality = quality,
            SettingsMsg::SetFps(fps) => self.settings.fps = fps,
            SettingsMsg::SetSystemAudio(on) => self.settings.system_audio = on,
            SettingsMsg::SetMicrophone(on) => self.settings.microphone = on,
            SettingsMsg::SetMicVolume(volume) => self.settings.mic_volume = volume as u8,
            SettingsMsg::SetAutoDiscovery(on) => self.settings.auto_discovery = on,
            SettingsMsg::SetFilmMode(on) => self.settings.film_mode = on,
            SettingsMsg::SetDeviceName(name) => self.settings.device_name = name,
            SettingsMsg::SetPort(port) => self.settings.port = port,
            SettingsMsg::Reset => {
                settings::reset();
                self.settings = settings::current();
                self.show(widgets);
            }
            SettingsMsg::Reload => {
                self.settings = settings::current();
                self.show(widgets);
                self.update_view(widgets, sender);
                return;
            }
        }

        settings::set(self.settings.clone());
        sender
            .output(SettingsOutput::Changed(self.settings.clone()))
            .ok();
        self.update_view(widgets, sender);
    }
}

impl SettingsPage {
    /// Writes the model into the controls, without that reading back as a
    /// change.
    fn show(&mut self, widgets: &SettingsPageWidgets) {
        self.loading = true;
        widgets.protocol.set_selected(match self.settings.protocol {
            Protocol::Auto => 0,
            Protocol::Miracast => 1,
            Protocol::Cast => 2,
        });
        widgets.quality.set_selected(match self.settings.quality {
            Quality::Max => 0,
            Quality::Ultra => 1,
            Quality::High => 2,
            Quality::Medium => 3,
            Quality::Low => 4,
        });
        widgets
            .fps
            .set_selected(if self.settings.fps > 30 { 1 } else { 0 });
        widgets.film_mode.set_active(self.settings.film_mode);
        widgets.system_audio.set_active(self.settings.system_audio);
        widgets.microphone.set_active(self.settings.microphone);
        widgets
            .mic_volume
            .set_value(self.settings.mic_volume as f64);
        widgets
            .auto_discovery
            .set_active(self.settings.auto_discovery);
        widgets.device_name.set_text(&self.settings.device_name);
        widgets.port.set_value(self.settings.port as f64);
        self.loading = false;
    }
}

fn string_list(items: &[String]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for item in items {
        list.append(item);
    }
    list
}

fn settings_path_line() -> String {
    match settings::path() {
        Some(path) => path.display().to_string(),
        None => tr!("Settings cannot be saved on this system."),
    }
}
