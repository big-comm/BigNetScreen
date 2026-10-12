//! The choices made before sharing, as rows of the sidebar: what to share,
//! the sound, the microphone, the picture and the delay.
//!
//! Each row shows its value and opens a card with the choices beside it
//! ([`super::hover_cards`]), on the window's overlay. The settings
//! page persists them: a change goes out as a whole [`Settings`], and the
//! saved value comes back through [`OptionsMsg::Settings`].

use relm4::adw::{self, prelude::*};
use relm4::gtk;
use relm4::prelude::*;

use nd_core::capture::SourceType;
use nd_core::latency::Profile;
use nd_core::settings::{self, Quality, Settings};

use super::hover_cards::{self, CardEvent, HoverCards};
use crate::tr;

/// What the receiver will show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Share {
    #[default]
    Screen,
    Window,
    Extra,
    /// Photos, videos and music played by the receiver itself.
    Files,
}

impl Share {
    /// The capture behind this choice; files are not captured.
    pub fn source(self) -> Option<SourceType> {
        match self {
            Share::Screen => Some(SourceType::Monitor),
            Share::Window => Some(SourceType::Window),
            Share::Extra => Some(SourceType::Virtual),
            Share::Files => None,
        }
    }

    fn label(self) -> String {
        match self {
            Share::Screen => tr!("Whole screen"),
            Share::Window => tr!("A window"),
            Share::Extra => tr!("Extra screen"),
            Share::Files => tr!("Files"),
        }
    }
}

/// Which sound goes with the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Off,
    All,
    Routed,
}

impl Sound {
    fn of(settings: &Settings) -> Self {
        match (settings.system_audio, settings.virtual_audio) {
            (false, _) => Sound::Off,
            (true, false) => Sound::All,
            (true, true) => Sound::Routed,
        }
    }
}

/// The resolution choices, largest first and the custom size last.
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

const FRAME_RATES: [u32; 5] = [24, 25, 30, 50, 60];

/// One sidebar row and the card it opens.
struct Row {
    row: gtk::ListBoxRow,
    value: gtk::Label,
    /// Why the row does not apply, shown in place of the value.
    reason: gtk::Label,
    /// The card: the description and the choices, held to the card's width.
    card: adw::Clamp,
}

/// A card's choices, each tile standing for one value.
struct Tiles<T>(Vec<(T, gtk::CheckButton)>);

impl<T: Copy + PartialEq + 'static> Tiles<T> {
    /// Tiles for `choices`, each sending `message` with its value when picked.
    fn new(
        choices: impl IntoIterator<Item = (T, String, String)>,
        sender: &ComponentSender<OptionsPanel>,
        message: fn(T) -> OptionsMsg,
    ) -> Self {
        let mut tiles = Vec::new();
        let mut first: Option<gtk::CheckButton> = None;
        for (value, name, detail) in choices {
            let tile = tile(&name, &detail);
            tile.set_group(first.as_ref());
            first.get_or_insert_with(|| tile.clone());
            let input = sender.input_sender().clone();
            tile.connect_toggled(move |tile| {
                if tile.is_active() {
                    input.emit(message(value));
                }
            });
            tiles.push((value, tile));
        }
        Self(tiles)
    }

    /// Marks the tile for `value`. The `toggled` it emits comes back as a
    /// message for the value already held, which changes nothing.
    fn show(&self, value: T) {
        if let Some((_, tile)) = self.0.iter().find(|(v, _)| *v == value) {
            tile.set_active(true);
        }
    }

    fn get(&self, value: T) -> Option<&gtk::CheckButton> {
        self.0
            .iter()
            .find(|(v, _)| *v == value)
            .map(|(_, tile)| tile)
    }

    fn column(&self) -> gtk::Box {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for (_, tile) in &self.0 {
            column.append(tile);
        }
        column
    }
}

pub struct OptionsPanel {
    settings: Settings,
    share: Share,
    virtual_available: bool,
    rows: [Row; 5],
    cards: HoverCards,
    share_tiles: Tiles<Share>,
    sound_tiles: Tiles<Sound>,
    microphone_tiles: Tiles<bool>,
    quality_tiles: Tiles<Quality>,
    delay_tiles: Tiles<Profile>,
    /// Why the extra screen cannot be chosen, under its tile.
    extra_reason: gtk::Label,
    open_sound: gtk::Button,
    mic_volume: gtk::Scale,
    custom_size: gtk::Box,
    custom_width: gtk::SpinButton,
    custom_height: gtk::SpinButton,
    fps_tiles: Tiles<u32>,
}

#[derive(Debug)]
pub enum OptionsMsg {
    /// The settings as saved, after a change from here or elsewhere.
    Settings(Settings),
    VirtualAvailable(bool),
    Card(CardEvent),
    Share(Share),
    Sound(Sound),
    Microphone(bool),
    MicVolume(u8),
    Quality(Quality),
    Width(u32),
    Height(u32),
    Fps(u32),
    Delay(Profile),
    OpenSound,
}

#[derive(Debug)]
pub enum OptionsOutput {
    Share(Share),
    Settings(Settings),
}

#[relm4::component(pub)]
impl Component for OptionsPanel {
    /// The window's overlay, where the cards open.
    type Init = gtk::Overlay;
    type Input = OptionsMsg;
    type Output = OptionsOutput;
    type CommandOutput = ();

    view! {
        gtk::ScrolledWindow {
            set_hscrollbar_policy: gtk::PolicyType::Never,
            set_vexpand: true,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_margin_all: 12,
                set_spacing: 6,

                #[name = "list"]
                gtk::ListBox {
                    set_selection_mode: gtk::SelectionMode::None,
                    add_css_class: "boxed-list",
                    add_css_class: "options-list",
                    connect_row_activated[sender] => move |_, row| {
                        if let Ok(index) = row.widget_name().parse() {
                            sender.input(OptionsMsg::Card(CardEvent::Activated(index)));
                        }
                    },
                },
                gtk::Label {
                    set_label: &tr!("Audio"),
                    set_xalign: 0.0,
                    set_margin_top: 12,
                    set_margin_start: 6,
                    add_css_class: "heading",
                },
                #[name = "sound_list"]
                gtk::ListBox {
                    set_selection_mode: gtk::SelectionMode::None,
                    add_css_class: "boxed-list",
                    add_css_class: "options-list",
                    connect_row_activated[sender] => move |_, row| {
                        if let Ok(index) = row.widget_name().parse() {
                            sender.input(OptionsMsg::Card(CardEvent::Activated(index)));
                        }
                    },
                },
            },
        }
    }

    fn init(
        layer: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let widgets = view_output!();

        let share_tiles = Tiles::new(
            [
                (
                    Share::Screen,
                    tr!("Everything on your screen, as you see it"),
                ),
                (
                    Share::Window,
                    tr!("Only the window you pick; the rest stays private"),
                ),
                (
                    Share::Extra,
                    tr!("A new screen beside yours, to use as a second monitor"),
                ),
                (
                    Share::Files,
                    tr!("Photos, videos and music, played by the receiver"),
                ),
            ]
            .map(|(share, detail)| (share, share.label(), detail)),
            &sender,
            OptionsMsg::Share,
        );
        let sound_tiles = Tiles::new(
            [
                (Sound::Off, tr!("No sound"), String::new()),
                (
                    Sound::All,
                    tr!("All computer sound"),
                    tr!("Everything this computer plays, notifications included"),
                ),
                (
                    Sound::Routed,
                    tr!("Only chosen applications"),
                    tr!("Adds a “BigNetScreen” output: only what you send to it is shared"),
                ),
            ],
            &sender,
            OptionsMsg::Sound,
        );
        let microphone_tiles = Tiles::new(
            [
                (false, tr!("Off"), String::new()),
                (true, tr!("On"), tr!("Mix your voice into what is sent")),
            ],
            &sender,
            OptionsMsg::Microphone,
        );
        let quality_tiles = Tiles::new(
            QUALITIES.map(|quality| (quality, quality_label(quality), String::new())),
            &sender,
            OptionsMsg::Quality,
        );
        let delay_tiles = Tiles::new(
            [
                (
                    Profile::Low,
                    tr!("Low"),
                    tr!("For using the computer on the big screen"),
                ),
                (Profile::Responsive, tr!("Balanced"), tr!("The default")),
                (Profile::Film, tr!("Film"), tr!("Smoothest, for watching")),
            ],
            &sender,
            OptionsMsg::Delay,
        );

        let extra_reason = caption(&tr!("This desktop cannot create an extra screen."));
        extra_reason.add_css_class("dim-label");
        extra_reason.set_wrap(true);
        let share = share_tiles.column();
        share.append(&extra_reason);

        let open_sound = gtk::Button::with_label(&tr!("Open sound settings"));
        open_sound.set_halign(gtk::Align::Start);
        open_sound.connect_clicked({
            let input = sender.input_sender().clone();
            move |_| input.emit(OptionsMsg::OpenSound)
        });
        let sound = sound_tiles.column();
        sound.append(&open_sound);

        let mic_volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 5.0);
        mic_volume.set_draw_value(true);
        mic_volume.set_value_pos(gtk::PositionType::Right);
        mic_volume.set_digits(0);
        mic_volume.update_property(&[gtk::accessible::Property::Label(&tr!("Microphone volume"))]);
        mic_volume.connect_value_changed({
            let input = sender.input_sender().clone();
            move |scale| input.emit(OptionsMsg::MicVolume(scale.value() as u8))
        });
        let microphone = microphone_tiles.column();
        microphone.append(&caption(&tr!("Microphone volume")));
        microphone.append(&mic_volume);

        let picture = gtk::Grid::builder()
            .column_spacing(6)
            .row_spacing(6)
            .column_homogeneous(true)
            .build();
        for (index, (_, tile)) in quality_tiles.0.iter().enumerate() {
            picture.attach(tile, (index % 2) as i32, (index / 2) as i32, 1, 1);
        }
        let custom_width = dimension(&tr!("Width (pixels)"), &sender, OptionsMsg::Width);
        let custom_height = dimension(&tr!("Height (pixels)"), &sender, OptionsMsg::Height);
        let custom_size = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        custom_size.append(&custom_width);
        custom_size.append(&gtk::Label::new(Some("×")));
        custom_size.append(&custom_height);
        let fps_tiles = Tiles::new(
            FRAME_RATES.map(|fps| (fps, fps.to_string(), String::new())),
            &sender,
            OptionsMsg::Fps,
        );
        let frame_rate = gtk::Box::builder().spacing(6).homogeneous(true).build();
        for (_, tile) in &fps_tiles.0 {
            frame_rate.append(tile);
        }
        let quality = gtk::Box::new(gtk::Orientation::Vertical, 6);
        quality.append(&picture);
        quality.append(&custom_size);
        quality.append(&caption(&tr!("Frames per second")));
        quality.append(&frame_rate);

        let rows = [
            row(
                "video-display-symbolic",
                &tr!("Share"),
                &tr!("What the receiver shows. Choose before picking where to share."),
                &share,
            ),
            row(
                "audio-volume-high-symbolic",
                &tr!("Sound"),
                &tr!("The sound that goes with the picture."),
                &sound,
            ),
            row(
                "audio-input-microphone-symbolic",
                &tr!("Microphone"),
                &tr!("Your voice, mixed into the sound that is sent."),
                &microphone,
            ),
            row(
                "view-fullscreen-symbolic",
                &tr!("Quality"),
                &tr!(
                    "The largest picture sent; a smaller screen is sent as it is. Many \
                     Chromecast models stay at 1080p and 30 frames per second."
                ),
                &quality,
            ),
            row(
                "preferences-system-time-symbolic",
                &tr!("Delay"),
                &tr!(
                    "Buffering smooths uneven playback and delays everything, the pointer \
                     included. Chromecast may ignore it."
                ),
                &delay_tiles.column(),
            ),
        ];
        // The picture's settings, then the sound's, as the sidebar groups
        // them; a row is found by its name, its place in `rows`.
        for (index, row) in rows.iter().enumerate() {
            row.row.set_widget_name(&index.to_string());
            match index {
                1 | 2 => widgets.sound_list.append(&row.row),
                _ => widgets.list.append(&row.row),
            }
        }
        let input = sender.input_sender().clone();
        let cards = HoverCards::new(
            &layer,
            rows.iter().map(|row| row.row.clone().upcast()).collect(),
            rows.iter().map(|row| row.card.clone().upcast()).collect(),
            move |event| input.emit(OptionsMsg::Card(event)),
        );
        // A scrolled sidebar moves the rows out from under their card.
        let input = sender.input_sender().clone();
        root.vadjustment()
            .connect_value_changed(move |_| input.emit(OptionsMsg::Card(CardEvent::Close)));

        let model = OptionsPanel {
            settings: settings::current(),
            share: Share::Screen,
            virtual_available: false,
            rows,
            cards,
            extra_reason,
            share_tiles,
            sound_tiles,
            microphone_tiles,
            quality_tiles,
            delay_tiles,
            open_sound,
            mic_volume,
            custom_size,
            custom_width,
            custom_height,
            fps_tiles,
        };
        model.show();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        let before = self.settings.clone();
        match message {
            OptionsMsg::Settings(settings) => {
                self.settings = settings;
                self.show();
                return;
            }
            OptionsMsg::VirtualAvailable(available) => {
                self.virtual_available = available;
                // A choice the desktop cannot honour falls back to the one
                // that always works, rather than failing at the receiver.
                if !available && self.share == Share::Extra {
                    self.share = Share::Screen;
                    sender.output(OptionsOutput::Share(self.share)).ok();
                }
                self.show();
                return;
            }
            OptionsMsg::Card(event) => {
                if self.cards.handle(event) {
                    self.focus_choice();
                }
                return;
            }
            OptionsMsg::Share(share) => {
                if share != self.share {
                    self.share = share;
                    sender.output(OptionsOutput::Share(share)).ok();
                    self.show();
                }
                return;
            }
            OptionsMsg::OpenSound => {
                open_sound_settings(root);
                return;
            }
            OptionsMsg::Sound(sound) => {
                self.settings.system_audio = sound != Sound::Off;
                self.settings.virtual_audio = sound == Sound::Routed;
            }
            OptionsMsg::Microphone(on) => self.settings.microphone = on,
            OptionsMsg::MicVolume(volume) => self.settings.mic_volume = volume,
            OptionsMsg::Quality(quality) => self.settings.quality = quality,
            OptionsMsg::Width(width) => {
                self.settings.custom_width = settings::valid_dimension(width)
            }
            OptionsMsg::Height(height) => {
                self.settings.custom_height = settings::valid_dimension(height)
            }
            OptionsMsg::Fps(fps) => self.settings.fps = fps,
            OptionsMsg::Delay(profile) => self.settings.latency = profile,
        }
        if self.settings != before {
            self.show();
            sender
                .output(OptionsOutput::Settings(self.settings.clone()))
                .ok();
        }
    }
}

impl OptionsPanel {
    /// Moves the focus to the picked choice in the open card.
    fn focus_choice(&self) {
        let s = &self.settings;
        let tile = match self.cards.open() {
            Some(0) => self.share_tiles.get(self.share),
            Some(1) => self.sound_tiles.get(Sound::of(s)),
            Some(2) => self.microphone_tiles.get(s.microphone),
            Some(3) => self.quality_tiles.get(s.quality),
            Some(4) => self.delay_tiles.get(s.latency),
            _ => None,
        };
        if let Some(tile) = tile {
            tile.grab_focus();
        }
    }

    /// Puts the current values on the rows and in the cards.
    fn show(&self) {
        let s = &self.settings;
        let [share, sound, microphone, quality, delay] = &self.rows;

        self.share_tiles.show(self.share);
        share.value.set_label(&self.share.label());
        if let Some(extra) = self.share_tiles.get(Share::Extra) {
            extra.set_sensitive(self.virtual_available);
        }
        self.extra_reason.set_visible(!self.virtual_available);

        let current = Sound::of(s);
        self.sound_tiles.show(current);
        self.open_sound.set_visible(current == Sound::Routed);
        sound.value.set_label(&match current {
            Sound::Off => tr!("Off"),
            Sound::All => tr!("All"),
            Sound::Routed => tr!("Chosen apps"),
        });
        set_applied(sound, current != Sound::Off);

        self.microphone_tiles.show(s.microphone);
        self.mic_volume.set_sensitive(s.microphone);
        if self.mic_volume.value() as u8 != s.mic_volume {
            self.mic_volume.set_value(f64::from(s.mic_volume));
        }
        microphone.value.set_label(&if s.microphone {
            format!("{} %", s.mic_volume)
        } else {
            tr!("Off")
        });
        set_applied(microphone, s.microphone);

        self.quality_tiles.show(s.quality);
        self.custom_size.set_visible(s.quality == Quality::Custom);
        for (spin, value) in [
            (&self.custom_width, s.custom_width),
            (&self.custom_height, s.custom_height),
        ] {
            if spin.value() as u32 != value {
                spin.set_value(f64::from(value));
            }
        }
        self.fps_tiles.show(s.fps);
        let height = match s.quality {
            Quality::Custom => s.custom_height,
            quality => quality.resolution().1,
        };
        quality
            .value
            .set_label(&format!("{height}p · {} FPS", s.fps));

        self.delay_tiles.show(s.latency);
        delay.value.set_label(&match s.latency {
            Profile::Low => tr!("Low"),
            Profile::Responsive => tr!("Balanced"),
            Profile::Film => tr!("Film"),
        });

        // Files are played by the receiver as they are: the capture
        // settings do not reach them.
        let files = self.share == Share::Files;
        for (index, row) in self.rows.iter().enumerate() {
            let applies = index == 0 || !files;
            row.row.set_sensitive(applies);
            row.value.set_visible(applies);
            row.reason.set_visible(!applies);
            let description = if applies {
                row.value.label()
            } else {
                row.reason.label()
            };
            row.row
                .update_property(&[gtk::accessible::Property::Description(&description)]);
        }
    }
}

/// A row of the sidebar: a round badge, the setting's name and its value,
/// opening a card with `description` over `choices`.
fn row(icon: &str, title: &str, description: &str, choices: &impl IsA<gtk::Widget>) -> Row {
    let badge = gtk::Image::builder()
        .icon_name(icon)
        .valign(gtk::Align::Center)
        .css_classes(["option-badge"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    let name = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["option-title"])
        .build();
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    let reason = gtk::Label::builder()
        .label(tr!("Not used for files"))
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .css_classes(["caption", "option-reason"])
        .build();
    text.append(&name);
    text.append(&reason);
    let value = gtk::Label::builder()
        .xalign(1.0)
        .valign(gtk::Align::Center)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(14)
        .css_classes(["option-value"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.append(&badge);
    content.append(&text);
    content.append(&value);
    let row = gtk::ListBoxRow::builder()
        .child(&content)
        .css_classes(["option-row"])
        .build();
    row.update_property(&[gtk::accessible::Property::Label(title)]);

    let about = gtk::Label::builder()
        .label(description)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(36)
        .css_classes(["card-description"])
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    content.append(&about);
    content.append(choices);
    // A long line wraps instead of widening the card.
    let card = adw::Clamp::builder()
        .child(&content)
        .maximum_size(hover_cards::CARD_WIDTH)
        .tightening_threshold(hover_cards::CARD_WIDTH)
        .build();
    Row {
        row,
        value,
        reason,
        card,
    }
}

/// A choice in a card: its name, with a line on what it does.
fn tile(name: &str, detail: &str) -> gtk::CheckButton {
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["option-name"])
            .build(),
    );
    if !detail.is_empty() {
        text.append(
            &gtk::Label::builder()
                .label(detail)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["option-detail"])
                .build(),
        );
    }
    let tile = gtk::CheckButton::builder()
        .child(&text)
        .css_classes(["option"])
        .build();
    // Named by its name alone: GTK would run both labels together.
    tile.update_property(&[gtk::accessible::Property::Label(name)]);
    if !detail.is_empty() {
        tile.update_property(&[gtk::accessible::Property::Description(detail)]);
    }
    tile
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(6)
        .css_classes(["caption-heading"])
        .build()
}

fn dimension(
    label: &str,
    sender: &ComponentSender<OptionsPanel>,
    message: fn(u32) -> OptionsMsg,
) -> gtk::SpinButton {
    let spin = gtk::SpinButton::new(
        Some(&gtk::Adjustment::new(
            1920.0,
            f64::from(settings::MIN_DIMENSION),
            f64::from(settings::MAX_DIMENSION),
            2.0,
            16.0,
            0.0,
        )),
        2.0,
        0,
    );
    spin.set_numeric(true);
    spin.set_hexpand(true);
    spin.update_property(&[gtk::accessible::Property::Label(label)]);
    let input = sender.input_sender().clone();
    spin.connect_value_changed(move |spin| input.emit(message(spin.value() as u32)));
    spin
}

/// Fills the row's badge while the setting adds something to the stream.
fn set_applied(row: &Row, applied: bool) {
    if applied {
        row.row.add_css_class("applied");
    } else {
        row.row.remove_css_class("applied");
    }
}

fn quality_label(quality: Quality) -> String {
    match quality {
        Quality::Max => tr!("Maximum (2160p)"),
        Quality::Ultra => tr!("Very high (1440p)"),
        Quality::High => tr!("High (1080p)"),
        Quality::Medium => tr!("Medium (720p)"),
        Quality::Low => tr!("Low (480p)"),
        Quality::Custom => tr!("Custom size"),
        sized => {
            let (width, height) = sized.resolution();
            format!("{width} × {height}")
        }
    }
}

/// Opens the desktop's sound settings, where an application's playback is
/// moved to the BigNetScreen output.
fn open_sound_settings(root: &gtk::ScrolledWindow) {
    let command = [
        ("systemsettings", "systemsettings kcm_pulseaudio"),
        ("gnome-control-center", "gnome-control-center sound"),
        ("pavucontrol-qt", "pavucontrol-qt"),
        ("pavucontrol", "pavucontrol"),
    ]
    .into_iter()
    .find(|(program, _)| gtk::glib::find_program_in_path(program).is_some());
    let result = command
        .ok_or_else(|| tr!("Open your desktop's sound settings to choose the BigNetScreen output."))
        .and_then(|(_, command)| {
            gtk::gio::AppInfo::create_from_commandline(
                command,
                None,
                gtk::gio::AppInfoCreateFlags::NONE,
            )
            // The display's context carries the activation token that lets
            // the compositor raise the opened window.
            .and_then(|app| app.launch(&[], Some(&root.display().app_launch_context())))
            .map_err(|err| err.to_string())
        });
    if let Err(reason) = result {
        let dialog = adw::AlertDialog::new(Some(&tr!("Sound settings")), Some(&reason));
        dialog.add_response("close", &tr!("Close"));
        dialog.present(Some(root));
    }
}
