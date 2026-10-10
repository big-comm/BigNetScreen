//! One file in the media grid: a toggle with its thumbnail, loaded in the
//! background through [`crate::pages::preview`].

use crate::pages::preview::{self, Preview};
use nd_chromecast::file_server::{MediaFile, MediaKind};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::gtk::{self, glib, prelude::*};
use std::path::PathBuf;

#[derive(Debug)]
pub struct MediaTile {
    file: MediaFile,
    pub(super) selected: bool,
    preview: Preview,
}

#[derive(Debug)]
pub enum TileMsg {
    Toggled(bool),
    SetSelected(bool),
}

#[derive(Debug)]
pub enum TileOutput {
    Selected(PathBuf, bool),
}

#[relm4::factory(pub)]
impl FactoryComponent for MediaTile {
    type Init = MediaFile;
    type Input = TileMsg;
    type Output = TileOutput;
    type CommandOutput = Result<Preview, String>;
    type ParentWidget = gtk::FlowBox;

    view! {
        gtk::ToggleButton {
            set_css_classes: &["media-tile", "flat"],
            #[watch]
            #[block_signal(toggle_handler)]
            set_active: self.selected,
            set_tooltip_text: Some(&self.file.path.to_string_lossy()),
            connect_toggled[sender] => move |button| {
                sender.input(TileMsg::Toggled(button.is_active()));
            } @toggle_handler,
            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 2,
                gtk::Overlay {
                    add_css_class: "thumb-frame",
                    set_overflow: gtk::Overflow::Hidden,
                    gtk::Box {
                        set_width_request: 170,
                        set_height_request: if self.file.kind == MediaKind::Video { 110 } else { 170 },
                    },
                    #[name = "thumb"]
                    add_overlay = &gtk::Picture {
                        set_content_fit: gtk::ContentFit::Cover,
                        set_can_shrink: true,
                        set_halign: gtk::Align::Fill,
                        set_valign: gtk::Align::Fill,
                        add_css_class: "thumb",
                        set_can_target: false,
                    },
                    add_overlay = &gtk::Image {
                        set_icon_name: Some(kind_icon(self.file.kind)),
                        set_pixel_size: 48,
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Center,
                        add_css_class: "thumb-fallback",
                        #[watch]
                        set_visible: self.preview.image.is_none(),
                        set_can_target: false,
                    },
                    add_overlay = &gtk::Image {
                        #[watch]
                        set_icon_name: if self.selected { Some("object-select-symbolic") } else { None },
                        set_halign: gtk::Align::Start,
                        set_valign: gtk::Align::Start,
                        add_css_class: "selection-mark",
                        set_can_target: false,
                    },
                    add_overlay = &gtk::Label {
                        #[watch]
                        set_label: &self.preview.duration.map(format_time).unwrap_or_default(),
                        #[watch]
                        set_visible: self.preview.duration.is_some(),
                        set_halign: gtk::Align::End,
                        set_valign: gtk::Align::End,
                        add_css_class: "duration-badge",
                        set_can_target: false,
                    },
                },
                gtk::Label {
                    set_label: &self.file.title(),
                    set_xalign: 0.0,
                    set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                    set_max_width_chars: 20,
                    add_css_class: "media-name",
                },
                gtk::Label {
                    #[watch]
                    set_label: &self.preview.artist.clone().unwrap_or_else(|| format!("{} · {}", glib::format_size(self.file.size), self.file.path.extension().unwrap_or_default().to_string_lossy().to_uppercase())),
                    set_xalign: 0.0,
                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                    set_max_width_chars: 20,
                    add_css_class: "media-detail",
                },
            },
        }
    }
    fn init_model(file: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            file,
            selected: false,
            preview: Preview::default(),
        }
    }
    fn init_widgets(
        &mut self,
        _index: &DynamicIndex,
        root: Self::Root,
        _returned: &<Self::ParentWidget as relm4::factory::FactoryView>::ReturnedWidget,
        sender: FactorySender<Self>,
    ) -> Self::Widgets {
        let widgets = view_output!();
        if self.file.kind == MediaKind::Music {
            root.add_css_class("music");
        }
        sender.oneshot_command(preview::load(self.file.path.clone(), self.file.kind));
        widgets
    }
    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: FactorySender<Self>,
    ) {
        match message {
            Ok(preview) => {
                if let Some(image) = &preview.image {
                    widgets.thumb.set_paintable(Some(&image.texture(360)));
                }
                self.preview = preview;
                self.update_view(widgets, sender);
            }
            Err(err) => tracing::debug!(%err, "no preview"),
        }
    }
    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            TileMsg::SetSelected(selected) => self.selected = selected,
            TileMsg::Toggled(selected) => {
                self.selected = selected;
                sender
                    .output(TileOutput::Selected(self.file.path.clone(), selected))
                    .ok();
            }
        }
    }
}

/// The generic icon for a kind of file, shown until its preview loads.
pub(super) fn kind_icon(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photo => "image-x-generic-symbolic",
        MediaKind::Video => "video-x-generic-symbolic",
        MediaKind::Music => "audio-x-generic-symbolic",
    }
}

pub(super) fn format_time(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    };
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
