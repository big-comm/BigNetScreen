//! One receiver as a list row, shared by the home and devices pages.

use nd_core::sink::SinkState;
use relm4::adw::{self, prelude::*};
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender, FactoryVecDeque};
use relm4::gtk;

use super::{DeviceEntry, latency_hint};
use crate::tr;

#[derive(Debug)]
pub struct DeviceRow {
    pub(super) entry: DeviceEntry,
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
            add_css_class: "device-row",
            set_use_markup: false,
            #[watch]
            set_title: &self.entry.name,
            #[watch]
            set_subtitle: &self.subtitle(),
            #[watch]
            set_activatable: self.entry.castable && !self.entry.state.is_busy(),

            add_prefix = &gtk::Image {
                #[watch]
                set_icon_name: Some(self.entry.icon()),
                set_pixel_size: 24,
                add_css_class: "device-icon",
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
                set_css_classes: &["badge", if self.entry.castable { self.entry.badge_class() } else { "discovery-only" }],

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
        if self.entry.kind == nd_core::sink::SinkKind::AirPlay {
            return tr!("This device is visible, but AirPlay sharing is not supported.");
        }
        if self.entry.state == SinkState::Error
            && self
                .entry
                .detail
                .ends_with(nd_chromecast::identity::IDENTITY_CHANGED)
        {
            return tr!("Did not prove it is the same device used before");
        }
        if self.entry.state == SinkState::Error && !self.entry.detail.is_empty() {
            return self.entry.detail.clone();
        }
        match latency_hint(self.entry.kind) {
            Some(hint) if self.entry.castable => format!("{} · {hint}", self.entry.subtitle()),
            _ => self.entry.subtitle(),
        }
    }
}

/// Brings the factory in line with `entries`, in place.
///
/// Rebuilding the list instead would make it flicker on every change and lose
/// the row under the pointer as the person is about to click it.
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
