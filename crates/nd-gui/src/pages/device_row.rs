//! One receiver as a list row, with the action it offers.

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
    /// The row's button was pressed.
    Act,
}

#[derive(Debug)]
pub enum DeviceRowOutput {
    Cast(String),
    TrustAgain(String),
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
            add_prefix = &gtk::Image {
                #[watch]
                set_icon_name: Some(self.entry.icon()),
                set_pixel_size: 24,
                add_css_class: "device-icon",
            },

            add_suffix = &gtk::Label {
                // Centred rather than filled: without this the label takes the
                // whole height of the row and the pill touches the separators.
                set_valign: gtk::Align::Center,
                set_vexpand: false,
                #[watch]
                set_label: &self.entry.badge(),
                #[watch]
                set_css_classes: &["badge", if self.entry.castable { self.entry.badge_class() } else { "discovery-only" }],
                // Only where it says more than the button beside it.
                #[watch]
                set_visible: self.action().is_none(),
            },

            add_suffix = &adw::Spinner {
                #[watch]
                set_visible: self.entry.state.is_busy(),
            },

            add_suffix = &gtk::Button {
                set_valign: gtk::Align::Center,
                #[watch]
                set_visible: self.action().is_some(),
                // A label child, so the accessible name below is the one read:
                // `set_label` would name the button after its text.
                #[wrap(Some)]
                set_child = &gtk::Label {
                    #[watch]
                    set_label: &match self.action() {
                        Some(RowAction::TrustAgain) => tr!("Trust again"),
                        _ => tr!("Share"),
                    },
                },
                #[watch]
                set_sensitive: self.entry.blocked.is_none(),
                #[watch]
                set_tooltip_text: match self.action() {
                    Some(RowAction::TrustAgain) => Some(tr!(
                        "Forget the device remembered for this receiver and trust the one that answers next"
                    )),
                    _ => self.entry.blocked.clone(),
                }.as_deref(),
                #[watch]
                update_property: &[gtk::accessible::Property::Label(&match self.action() {
                    Some(RowAction::TrustAgain) => tr!("Trust {} again"),
                    _ => tr!("Share to {}"),
                }.replace("{}", &self.entry.name))],
                connect_clicked => DeviceRowMsg::Act,
            },
        }
    }

    fn init_model(entry: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self { entry }
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        match message {
            DeviceRowMsg::Update(entry) => self.entry = entry,
            DeviceRowMsg::Act => {
                let id = self.entry.id.clone();
                let output = match self.action() {
                    Some(RowAction::Cast) => DeviceRowOutput::Cast(id),
                    Some(RowAction::TrustAgain) => DeviceRowOutput::TrustAgain(id),
                    None => return,
                };
                sender.output(output).ok();
            }
        }
    }
}

/// What the row's button does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Cast,
    TrustAgain,
}

/// The receiver was refused for proving another identity.
pub fn identity_refused(entry: &DeviceEntry) -> bool {
    entry.state == SinkState::Error
        && entry
            .detail
            .ends_with(nd_chromecast::identity::IDENTITY_CHANGED)
}

impl DeviceRow {
    fn action(&self) -> Option<RowAction> {
        if identity_refused(&self.entry) {
            Some(RowAction::TrustAgain)
        } else if self.entry.castable
            && !self.entry.state.is_busy()
            && self.entry.state != SinkState::Streaming
        {
            Some(RowAction::Cast)
        } else {
            None
        }
    }

    /// Protocol, address, and what the receiver is doing or why it failed, when
    /// known.
    fn subtitle(&self) -> String {
        if self.entry.kind == nd_core::sink::SinkKind::AirPlay {
            return tr!("This device is visible, but AirPlay sharing is not supported.");
        }
        if identity_refused(&self.entry) {
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
