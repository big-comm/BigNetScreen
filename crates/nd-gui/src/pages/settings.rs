//! The preferences, and only the ones that change something.
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

use nd_core::latency::Profile;
use nd_core::settings::{self, Protocol, Quality, Settings};

use crate::tr;

/// How long the controls have to settle before the file is written.
///
/// A slider emits dozens of values per second and each write ends in an
/// `fsync`; doing that on the GTK thread stutters the window while dragging.
/// In memory the change is immediate, only the disk waits.
const SAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// The frame rates the chooser offers, in its order.
const FRAME_RATES: [u32; 5] = [24, 25, 30, 50, 60];

pub struct SettingsPage {
    settings: Settings,
    /// The write scheduled for when the controls settle, if any.
    pending_write: std::rc::Rc<std::cell::Cell<Option<gtk::glib::SourceId>>>,
}

#[derive(Debug)]
pub enum SettingsMsg {
    SetProtocol(Protocol),
    SetQuality(Quality),
    SetFps(u32),
    SetWidth(u32),
    SetHeight(u32),
    SetHardwareEncoding(bool),
    SetSystemAudio(bool),
    SetMicrophone(bool),
    SetVirtualAudio(bool),
    SetMicVolume(f64),
    SetAutoDiscovery(bool),
    /// The same preference, changed on another page: the switch follows.
    SyncAutoDiscovery(bool),
    SetLatency(Profile),
    SetDeviceName(String),
    SetPort(u16),
    Reset,
    ResetConfirmed,
    OpenSound,
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

            adw::Clamp {
                set_maximum_size: 760,

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_all: 28,
                    add_css_class: "settings-page",
                    set_spacing: 6,

            gtk::Box {
                add_css_class: "page-heading",
                set_spacing: 18,
                gtk::Image { set_icon_name: Some("preferences-system-symbolic"), set_pixel_size: 32, add_css_class: "page-icon", set_valign: gtk::Align::Center },
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_valign: gtk::Align::Center,
                    set_spacing: 6,
                    gtk::Label { set_label: &tr!("Settings"), set_xalign: 0.0, add_css_class: "page-title" },
                    gtk::Label { set_label: &tr!("These apply to your next session."), set_xalign: 0.0, set_wrap: true, add_css_class: "page-subtitle" },
                },
            },

                    adw::PreferencesGroup {
                        set_title: &tr!("Streaming"),
                        set_description: Some(&tr!(
                            "A receiver can accept less than you choose: many Chromecast models stay at 1080p and 30 FPS."
                        )),
                        set_margin_top: 12,



                        #[name = "quality"]
                        adw::ComboRow {
                            set_title: &tr!("Resolution limit"),
                            add_prefix = &gtk::Image { set_icon_name: Some("view-fullscreen-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_use_subtitle: true,
                            set_tooltip_text: Some(&tr!(
                                "A ceiling: a smaller screen is sent as it is. Above 1080p \
                                 is sent only if the receiver accepts it — many Chromecast \
                                 models mirror at 1080p and will hold it there."
                            )),
                            set_model: Some(&string_list(&QUALITIES.map(quality_label))),
                            connect_selected_notify[sender] => move |row| {
                                let quality = QUALITIES.get(row.selected() as usize);
                                sender.input(SettingsMsg::SetQuality(quality.copied().unwrap_or_default()));
                            } @quality_handler,
                        },

                        adw::ActionRow {
                            set_title: &tr!("Width (pixels)"),
                            #[watch]
                            set_visible: model.settings.quality == Quality::Custom,
                            #[name = "custom_width"]
                            add_suffix = &gtk::SpinButton {
                                update_property: &[gtk::accessible::Property::Label(&tr!("Width (pixels)"))],
                                set_valign: gtk::Align::Center,
                                set_adjustment: &dimension_adjustment(1920),
                                set_numeric: true,
                                connect_value_changed[sender] => move |spin| { sender.input(SettingsMsg::SetWidth(spin.value() as u32)); } @custom_width_handler,
                            },
                        },
                        adw::ActionRow {
                            set_title: &tr!("Height (pixels)"),
                            set_subtitle: &tr!("Aspect ratio is preserved. Miracast uses a supported mode within these limits."),
                            #[watch]
                            set_visible: model.settings.quality == Quality::Custom,
                            #[name = "custom_height"]
                            add_suffix = &gtk::SpinButton {
                                update_property: &[gtk::accessible::Property::Label(&tr!("Height (pixels)"))],
                                set_valign: gtk::Align::Center,
                                set_adjustment: &dimension_adjustment(1080),
                                set_numeric: true,
                                connect_value_changed[sender] => move |spin| { sender.input(SettingsMsg::SetHeight(spin.value() as u32)); } @custom_height_handler,
                            },
                        },



                        // One choice, not two switches: the lowest delay and the
                        // smoothest playback are ends of the same scale, and
                        // offering each as its own toggle would let someone ask
                        // for both and leave the product to decide in silence.
                        #[name = "latency"]
                        adw::ComboRow {
                            set_title: &tr!("Delay"),
                            add_prefix = &gtk::Image { set_icon_name: Some("video-x-generic-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_use_subtitle: true,
                            set_tooltip_text: Some(&tr!(
                                "Buffering smooths uneven playback and delays everything, the \
                                 pointer included. On Miracast it is applied here; on \
                                 Chromecast it is a request the receiver may ignore."
                            )),
                            set_model: Some(&string_list(&[
                                tr!("Low — for using the computer on the big screen"),
                                tr!("Balanced — the default"),
                                tr!("Film — smoothest, for watching"),
                            ])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetLatency(match row.selected() {
                                    0 => Profile::Low,
                                    2 => Profile::Film,
                                    _ => Profile::Responsive,
                                }));
                            } @latency_handler,
                        },

                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Audio"),
                        set_margin_top: 18,

                        #[name = "system_audio"]
                        adw::SwitchRow {
                            set_title: &tr!("Include system audio"),
                            add_prefix = &gtk::Image { set_icon_name: Some("audio-volume-high-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_subtitle: &tr!("Include sound from applications. Choose below whether to share all sound or only routed applications."),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetSystemAudio(row.is_active()));
                            } @system_audio_handler,
                        },

                        #[name = "virtual_audio"]
                        adw::SwitchRow {
                            set_title: &tr!("Only applications routed to BigNetScreen"),
                            add_prefix = &gtk::Image { set_icon_name: Some("audio-card-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            // The name has to match what the sound settings
                            // show, or the instruction sends people looking for
                            // a device they will not find.
                            set_subtitle: &tr!("Adds a “BigNetScreen” output. Send that application's sound to it and nothing else leaves this computer"),
                            // It narrows the switch above; on its own it means
                            // nothing, so it does not stay pressable when there
                            // is no system audio to narrow.
                            #[watch]
                            set_sensitive: model.settings.system_audio,
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetVirtualAudio(row.is_active()));
                            } @virtual_audio_handler,
                        },

                        adw::ActionRow {
                            set_title: &tr!("Choose the sound to share"),
                            set_subtitle: &tr!("In your system sound settings, move each application's playback to the BigNetScreen output. Microphone audio is controlled separately below."),
                            #[watch]
                            set_visible: model.settings.system_audio && model.settings.virtual_audio,
                            add_suffix = &gtk::Button {
                                set_label: &tr!("Open sound settings"),
                                set_valign: gtk::Align::Center,
                                connect_clicked => SettingsMsg::OpenSound,
                            },
                        },

                        #[name = "microphone"]
                        adw::SwitchRow {
                            set_title: &tr!("Include the microphone"),
                            add_prefix = &gtk::Image { set_icon_name: Some("audio-input-microphone-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_subtitle: &tr!("Mix your voice into what is sent"),
                            connect_active_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetMicrophone(row.is_active()));
                            } @microphone_handler,
                        },

                        adw::ActionRow {
                            set_title: &tr!("Microphone volume"),
                            add_prefix = &gtk::Image { set_icon_name: Some("audio-volume-low-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            #[watch]
                            set_sensitive: model.settings.microphone,

                            #[name = "mic_volume"]
                            add_suffix = &gtk::Scale {
                                update_property: &[gtk::accessible::Property::Label(&tr!("Microphone volume"))],
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
                                } @mic_volume_handler,
                            },
                        },
                    },

                    adw::PreferencesGroup {
                        set_title: &tr!("Discovery and devices"),
                        set_margin_top: 18,

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
                        #[name = "fps"]
                        adw::ComboRow {
                            set_title: &tr!("Frame rate"),
                            add_prefix = &gtk::Image { set_icon_name: Some("view-list-symbolic"), set_pixel_size: 22, add_css_class: "settings-icon" },
                            set_use_subtitle: true,
                            set_tooltip_text: Some(&tr!(
                                "60 is smoother and costs about half as much again in \
                                 bandwidth. Many Chromecast models mirror at 30 and will \
                                 hold it there."
                            )),
                            set_model: Some(&string_list(&[tr!("24 FPS"), tr!("25 FPS"), tr!("30 FPS"), tr!("50 FPS"), tr!("60 FPS")])),
                            connect_selected_notify[sender] => move |row| {
                                sender.input(SettingsMsg::SetFps(
                                    FRAME_RATES.get(row.selected() as usize).copied().unwrap_or(30),
                                ));
                            } @fps_handler,
                        },
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
            SettingsMsg::OpenSound => {
                let command = [
                    ("systemsettings", "systemsettings kcm_pulseaudio"),
                    ("gnome-control-center", "gnome-control-center sound"),
                    ("pavucontrol-qt", "pavucontrol-qt"),
                    ("pavucontrol", "pavucontrol"),
                ]
                .into_iter()
                .find(|(program, _)| gtk::glib::find_program_in_path(program).is_some());
                let result = command
                    .ok_or_else(|| {
                        tr!("Open your desktop's sound settings to choose the BigNetScreen output.")
                    })
                    .and_then(|(_, command)| {
                        gtk::gio::AppInfo::create_from_commandline(
                            command,
                            None,
                            gtk::gio::AppInfoCreateFlags::NONE,
                        )
                        // The display's context carries the activation token
                        // that lets the compositor raise the opened window.
                        .and_then(|app| app.launch(&[], Some(&root.display().app_launch_context())))
                        .map_err(|err| err.to_string())
                    });
                if let Err(reason) = result {
                    let dialog = adw::AlertDialog::new(Some(&tr!("Sound settings")), Some(&reason));
                    dialog.add_response("close", &tr!("Close"));
                    dialog.present(Some(root));
                }
                return;
            }
            SettingsMsg::SetProtocol(protocol) => self.settings.protocol = protocol,
            SettingsMsg::SetQuality(quality) => self.settings.quality = quality,
            SettingsMsg::SetFps(fps) => self.settings.fps = fps,
            SettingsMsg::SetWidth(width) => {
                self.settings.custom_width = settings::valid_dimension(width)
            }
            SettingsMsg::SetHeight(height) => {
                self.settings.custom_height = settings::valid_dimension(height)
            }
            SettingsMsg::SetHardwareEncoding(on) => self.settings.hardware_encoding = on,
            SettingsMsg::SetSystemAudio(on) => self.settings.system_audio = on,
            SettingsMsg::SetVirtualAudio(on) => self.settings.virtual_audio = on,
            SettingsMsg::SetMicrophone(on) => self.settings.microphone = on,
            SettingsMsg::SetMicVolume(volume) => self.settings.mic_volume = volume as u8,
            SettingsMsg::SetAutoDiscovery(on) => self.settings.auto_discovery = on,
            SettingsMsg::SyncAutoDiscovery(on) => {
                self.settings.auto_discovery = on;
                widgets
                    .auto_discovery
                    .block_signal(&widgets.auto_discovery_handler);
                widgets.auto_discovery.set_active(on);
                widgets
                    .auto_discovery
                    .unblock_signal(&widgets.auto_discovery_handler);
            }
            SettingsMsg::SetLatency(profile) => self.settings.latency = profile,
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
        let handlers: [(&gtk::glib::Object, &gtk::glib::SignalHandlerId); 14] = [
            (w.protocol.upcast_ref(), &w.protocol_handler),
            (w.quality.upcast_ref(), &w.quality_handler),
            (w.custom_width.upcast_ref(), &w.custom_width_handler),
            (w.custom_height.upcast_ref(), &w.custom_height_handler),
            (w.fps.upcast_ref(), &w.fps_handler),
            (w.latency.upcast_ref(), &w.latency_handler),
            (
                w.hardware_encoding.upcast_ref(),
                &w.hardware_encoding_handler,
            ),
            (w.system_audio.upcast_ref(), &w.system_audio_handler),
            (w.virtual_audio.upcast_ref(), &w.virtual_audio_handler),
            (w.microphone.upcast_ref(), &w.microphone_handler),
            (w.mic_volume.upcast_ref(), &w.mic_volume_handler),
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
        widgets.quality.set_selected(
            QUALITIES
                .iter()
                .position(|quality| *quality == self.settings.quality)
                .unwrap_or_default() as u32,
        );
        widgets.fps.set_selected(
            FRAME_RATES
                .iter()
                .position(|fps| *fps == self.settings.fps)
                .unwrap_or(2) as u32,
        );
        widgets.latency.set_selected(match self.settings.latency {
            Profile::Low => 0,
            Profile::Responsive => 1,
            Profile::Film => 2,
        });
        widgets
            .hardware_encoding
            .set_active(self.settings.hardware_encoding);
        widgets.system_audio.set_active(self.settings.system_audio);
        widgets
            .virtual_audio
            .set_active(self.settings.virtual_audio);
        widgets.microphone.set_active(self.settings.microphone);
        widgets
            .mic_volume
            .set_value(self.settings.mic_volume as f64);
        widgets
            .auto_discovery
            .set_active(self.settings.auto_discovery);
        widgets.device_name.set_text(&self.settings.device_name);
        widgets.port.set_value(self.settings.port as f64);
        widgets
            .custom_width
            .set_value(self.settings.custom_width as f64);
        widgets
            .custom_height
            .set_value(self.settings.custom_height as f64);

        for (object, handler) in handlers {
            object.unblock_signal(handler);
        }
    }
}

/// The resolution choices, largest first and the custom size last; a row's
/// index is a position here.
const QUALITIES: [Quality; 11] = [
    Quality::Max,
    Quality::Ultrawide,
    Quality::Wqxga,
    Quality::Ultra,
    Quality::Wuxga,
    Quality::High,
    Quality::HdPlus,
    Quality::Wxga,
    Quality::Medium,
    Quality::Low,
    Quality::Custom,
];

fn quality_label(quality: Quality) -> String {
    match quality {
        Quality::Max => tr!("Maximum (2160p)"),
        Quality::Ultra => tr!("Very high (1440p)"),
        Quality::High => tr!("High (1080p)"),
        Quality::Medium => tr!("Medium (720p)"),
        Quality::Low => tr!("Low (480p)"),
        Quality::Custom => tr!("Custom resolution"),
        sized => {
            let (width, height) = sized.resolution();
            format!("{width} × {height}")
        }
    }
}

fn dimension_adjustment(value: u32) -> gtk::Adjustment {
    gtk::Adjustment::new(
        f64::from(value),
        f64::from(settings::MIN_DIMENSION),
        f64::from(settings::MAX_DIMENSION),
        2.0,
        16.0,
        0.0,
    )
}

fn string_list(items: &[String]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for item in items {
        list.append(item);
    }
    list
}
