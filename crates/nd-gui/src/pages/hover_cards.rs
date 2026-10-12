//! Cards that open beside their row as the pointer reaches it.
//!
//! Each card is a child of the window's overlay, placed beside its row and
//! joined to it by a bridge, so the row and its card read as one shape. The
//! pointer opens a card and may travel into it: leaving the row or the card
//! starts a short grace before it closes, and a row crossed on the way takes
//! over only if the pointer rests on it. A click or Enter opens a card for the
//! keyboard: the owner focuses it, and Escape, a click outside or the focus
//! leaving closes it; the pointer never replaces it.
//!
//! The widgets' handlers only report [`CardEvent`]s; the owner passes them back
//! to [`HoverCards::handle`], so no handler holds a widget.

use std::rc::Rc;
use std::time::Duration;

use relm4::gtk::{self, glib, prelude::*};

/// How long the pointer rests on another row before its card replaces the
/// open one.
const SWITCH_DELAY: Duration = Duration::from_millis(150);
/// How long a card stays after the pointer leaves it and its row.
const CLOSE_DELAY: Duration = Duration::from_millis(300);
/// The space between the row and its card, crossed by the bridge.
const GAP: i32 = 12;
/// The ring's distance from the edge and its width, as the CSS draws it.
const RING_INSET: i32 = 3;
const RING: i32 = 2;
/// The card's width; the owner clamps its content to it.
pub const CARD_WIDTH: i32 = 340;

#[derive(Debug, Clone, Copy)]
pub enum CardEvent {
    /// The pointer reached row `n`.
    Entered(usize),
    Left(usize),
    CardEntered,
    CardLeft,
    /// Row `n` was clicked or activated with the keyboard.
    Activated(usize),
    Escape,
    /// The keyboard focus left the open card.
    FocusLeft,
    /// A press anywhere in the window, at these overlay coordinates.
    Pressed(f64, f64),
    /// The timer set by the last pointer move ran out.
    Settled,
    /// The rows moved under the card (the sidebar scrolled).
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Opener {
    Pointer,
    Keyboard,
}

pub struct HoverCards {
    layer: gtk::Overlay,
    rows: Vec<gtk::Widget>,
    cards: Vec<gtk::Widget>,
    bridge: gtk::Box,
    open: Option<(usize, Opener)>,
    /// The row under the pointer, if any.
    hovered: Option<usize>,
    /// What the running timer does: open this row's card, or close.
    settle_to: Option<usize>,
    timer: Option<glib::SourceId>,
    emit: Rc<dyn Fn(CardEvent)>,
}

impl HoverCards {
    /// Puts `cards` on `layer`, card `n` belonging to `rows[n]`.
    pub fn new(
        layer: &gtk::Overlay,
        rows: Vec<gtk::Widget>,
        cards: Vec<gtk::Widget>,
        emit: impl Fn(CardEvent) + 'static,
    ) -> Self {
        let emit: Rc<dyn Fn(CardEvent)> = Rc::new(emit);
        for (index, row) in rows.iter().enumerate() {
            let motion = gtk::EventControllerMotion::new();
            let enter = emit.clone();
            motion.connect_enter(move |_, _, _| enter(CardEvent::Entered(index)));
            let leave = emit.clone();
            motion.connect_leave(move |_| leave(CardEvent::Left(index)));
            row.add_controller(motion);
            row.update_state(&[gtk::accessible::State::Expanded(Some(false))]);
        }
        for card in &cards {
            card.set_visible(false);
            card.set_halign(gtk::Align::Start);
            card.set_valign(gtk::Align::Start);
            card.add_css_class("option-card");
            let motion = gtk::EventControllerMotion::new();
            let enter = emit.clone();
            motion.connect_enter(move |_, _, _| enter(CardEvent::CardEntered));
            let leave = emit.clone();
            motion.connect_leave(move |_| leave(CardEvent::CardLeft));
            card.add_controller(motion);
            let keys = gtk::EventControllerKey::new();
            let escape = emit.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    escape(CardEvent::Escape);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            card.add_controller(keys);
            let focus = gtk::EventControllerFocus::new();
            let left = emit.clone();
            focus.connect_leave(move |_| left(CardEvent::FocusLeft));
            card.add_controller(focus);
            layer.add_overlay(card);
        }
        let bridge = gtk::Box::builder()
            .visible(false)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .can_target(false)
            .css_classes(["option-bridge"])
            .build();
        layer.add_overlay(&bridge);

        // Observes presses without claiming them, so the press still reaches
        // what is under it.
        let presses = gtk::GestureClick::new();
        presses.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pressed = emit.clone();
        presses.connect_pressed(move |_, _, x, y| pressed(CardEvent::Pressed(x, y)));
        layer.add_controller(presses);

        Self {
            layer: layer.clone(),
            rows,
            cards,
            bridge,
            open: None,
            hovered: None,
            settle_to: None,
            timer: None,
            emit,
        }
    }

    /// The row whose card is open.
    pub fn open(&self) -> Option<usize> {
        self.open.map(|(index, _)| index)
    }

    /// Acts on an event; `true` when a card was opened for the keyboard, for
    /// the owner to move the focus into it.
    pub fn handle(&mut self, event: CardEvent) -> bool {
        let by_pointer = matches!(self.open, Some((_, Opener::Pointer)));
        match event {
            CardEvent::Entered(index) => {
                self.hovered = Some(index);
                match self.open {
                    None => self.show(index, Opener::Pointer),
                    Some((open, Opener::Pointer)) if open == index => self.cancel(),
                    Some((_, Opener::Pointer)) => self.schedule(SWITCH_DELAY, Some(index)),
                    Some((_, Opener::Keyboard)) => {}
                }
            }
            CardEvent::Left(index) => {
                if self.hovered == Some(index) {
                    self.hovered = None;
                }
                if by_pointer {
                    self.schedule(CLOSE_DELAY, None);
                }
            }
            CardEvent::CardEntered => {
                if by_pointer {
                    self.cancel();
                }
            }
            CardEvent::CardLeft => {
                if by_pointer && self.hovered.is_none() {
                    self.schedule(CLOSE_DELAY, None);
                }
            }
            CardEvent::Settled => {
                // The source has run and is gone; only its id is left.
                self.timer = None;
                if by_pointer {
                    match self.settle_to.or(self.hovered) {
                        Some(index) => self.show(index, Opener::Pointer),
                        None => self.close(),
                    }
                }
            }
            CardEvent::Activated(index) => {
                self.show(index, Opener::Keyboard);
                return true;
            }
            CardEvent::Escape => {
                if let Some(index) = self.open() {
                    self.close();
                    self.rows[index].grab_focus();
                }
            }
            CardEvent::FocusLeft => {
                if matches!(self.open, Some((_, Opener::Keyboard))) {
                    self.close();
                }
            }
            CardEvent::Pressed(x, y) => {
                if let Some(index) = self.open()
                    && !self.contains(&self.cards[index], x, y)
                    && !self.contains(&self.rows[index], x, y)
                {
                    self.close();
                }
            }
            CardEvent::Close => self.close(),
        }
        false
    }

    pub fn close(&mut self) {
        self.cancel();
        if let Some((index, _)) = self.open.take() {
            self.cards[index].set_visible(false);
            self.bridge.set_visible(false);
            let row = &self.rows[index];
            row.remove_css_class("card-open");
            row.update_state(&[gtk::accessible::State::Expanded(Some(false))]);
            self.layer.remove_css_class("cards-open");
        }
    }

    /// Opens row `index`'s card beside it, held inside the window.
    fn show(&mut self, index: usize, opener: Opener) {
        self.cancel();
        if self.open.map(|(open, _)| open) != Some(index) {
            self.close();
        }
        let (row, card) = (&self.rows[index], &self.cards[index]);
        let Some(bounds) = row.compute_bounds(&self.layer) else {
            return;
        };
        let (x, y) = (bounds.x() as i32, bounds.y() as i32);
        let (width, height) = (bounds.width() as i32, bounds.height() as i32);
        let (_, card_width, _, _) = card.measure(gtk::Orientation::Horizontal, -1);
        let (_, card_height, _, _) = card.measure(gtk::Orientation::Vertical, card_width);
        let layer_height = self.layer.height();
        let top = y.min(layer_height - card_height - GAP).max(0);
        // Margins count from the start edge, which is the right one in a
        // right-to-left layout.
        let rtl = self.layer.direction() == gtk::TextDirection::Rtl;
        let from_start = |left: i32, span: i32| {
            if rtl {
                self.layer.width() - left - span
            } else {
                left
            }
        };
        let card_left = if rtl {
            x - GAP - card_width
        } else {
            x + width + GAP
        };
        card.set_margin_start(from_start(card_left, card_width));
        card.set_margin_top(top);
        if top == y {
            card.add_css_class("joined");
        } else {
            card.remove_css_class("joined");
        }
        card.set_visible(true);

        // Over both rings' facing sides, so they run on into each other.
        let reach = RING_INSET + RING;
        let bridge_left = if rtl {
            x - GAP - reach
        } else {
            x + width - reach
        };
        let bridge_width = GAP + 2 * reach;
        self.bridge
            .set_margin_start(from_start(bridge_left, bridge_width));
        self.bridge.set_margin_top(y + RING_INSET);
        self.bridge
            .set_size_request(bridge_width, height - 2 * RING_INSET);
        self.bridge.set_visible(true);

        row.add_css_class("card-open");
        row.update_state(&[gtk::accessible::State::Expanded(Some(true))]);
        // While a card is open no other row lights up under the pointer.
        self.layer.add_css_class("cards-open");
        self.open = Some((index, opener));
    }

    fn contains(&self, widget: &gtk::Widget, x: f64, y: f64) -> bool {
        widget
            .compute_bounds(&self.layer)
            .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)))
    }

    fn schedule(&mut self, delay: Duration, settle_to: Option<usize>) {
        self.cancel();
        self.settle_to = settle_to;
        let emit = self.emit.clone();
        self.timer = Some(glib::timeout_add_local_once(delay, move || {
            emit(CardEvent::Settled)
        }));
    }

    fn cancel(&mut self) {
        if let Some(timer) = self.timer.take() {
            big_gtk_kit::source::remove(timer);
        }
    }
}

impl Drop for HoverCards {
    fn drop(&mut self) {
        self.cancel();
    }
}
