use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gio, glib};
use mailparse::MailHeaderMap;

use crate::composer;
use crate::favorites::{self, Favorite};
use crate::highlight;
use crate::inbox_page::{apply_star_state, new_star_button};
use crate::lore;
use crate::remote_page::RemoteContent;

struct Mail {
    subject: String,
    from: String,
    to: String,
    to_addrs: Vec<String>,
    cc: Option<String>,
    cc_addrs: Vec<String>,
    date: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    body: String,
    /// The message's raw RFC 5322 text (without the mbox "From " line),
    /// shown by the per-mail Raw view.
    raw: String,
}

// One message as a GListModel item. Holds the parsed Mail behind an Rc so the
// store owns each message once and the factory hands rows a cheap handle
// rather than cloning body strings on every bind.
glib::wrapper! {
    pub struct MessageObject(ObjectSubclass<imp::MessageObject>);
}

impl MessageObject {
    fn new(mail: Rc<Mail>, is_op: bool) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().mail.set(mail).ok();
        obj.imp().is_op.set(is_op);
        obj
    }

    fn message(&self) -> Rc<Mail> {
        self.imp()
            .mail
            .get()
            .expect("MessageObject mail set")
            .clone()
    }

    /// Whether this message is the thread's OP. Tracked on the object rather
    /// than derived from the row's position, because the single view shows one
    /// message at position 0 that may well be a reply — its card must still
    /// carry its own Subject row.
    fn is_op(&self) -> bool {
        self.imp().is_op.get()
    }
}

// The row widget for one message: its header card stacked over its body.
// GtkListView creates and binds every row of a <=205-item model up front
// (its widget window is a hardcoded 205 items), so both construction and
// bind must be next to free: the row starts as an empty shell holding
// nothing but its seeded height, and the whole machinery — header card,
// TextView, highlight, context menu, action group — is built lazily by the
// first fill (see message_fill_step for when that happens).
glib::wrapper! {
    pub struct MessageRow(ObjectSubclass<imp::MessageRow>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

thread_local! {
    /// Exact pixel height of one body line, measured from the first filled
    /// TextView (0 until known). With it, the seeded heights are
    /// pixel-perfect for plain lines — bodies are monospace and never wrap —
    /// so filling a row no longer shifts scroll geometry under the reader.
    static BODY_LINE_HEIGHT: Cell<i32> = const { Cell::new(0) };
    /// Exact pixel heights of a message's header card, measured from the
    /// first filled card of each kind (0 until known). A card's structure is
    /// fixed per kind — nothing in it wraps, and the Details expander starts
    /// collapsed — so one measurement covers every card of that kind.
    static HEADER_HEIGHT_OP: Cell<i32> = const { Cell::new(0) };
    static HEADER_HEIGHT_REPLY: Cell<i32> = const { Cell::new(0) };
}

/// The measured body line height, or a conservative floor while no body has
/// been filled yet. The floor must underestimate (a too-large seed would pad
/// short rows forever); any real monospace line is taller than 10px.
fn body_line_height() -> i32 {
    let height = BODY_LINE_HEIGHT.with(Cell::get);
    if height > 0 { height } else { 10 }
}

/// The measured header-card height for the OP or a reply, or a conservative
/// floor while no card of that kind has been filled yet. Like the line
/// height, the floor must underestimate; real cards run past 150px (the
/// reply card is taller by its Subject row).
fn header_height(is_op: bool) -> i32 {
    let height = if is_op {
        HEADER_HEIGHT_OP.with(Cell::get)
    } else {
        HEADER_HEIGHT_REPLY.with(Cell::get)
    };
    if height > 0 {
        height
    } else if is_op {
        90
    } else {
        120
    }
}

/// Vertical pixels of a filled row that are neither header card nor body
/// lines: the content box's top margin (12) and header/body spacing (12),
/// plus the body view's own vertical margins (12 + 12).
const ROW_CHROME_HEIGHT: i32 = 48;

impl MessageRow {
    fn new(
        composer: &composer::Composer,
        overlay: &adw::ToastOverlay,
        list: &str,
        fav_hub: &FavoriteHub,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.composer.set(composer.clone()).ok();
        imp.list.set(list.to_string()).ok();
        imp.fav_hub.set(fav_hub.clone()).ok();
        // Weak: the overlay is this row's ancestor, and a strong handle here
        // would cycle the whole page tree alive after it is popped.
        imp.overlay.set(Some(overlay));
        obj
    }

    fn set_message(&self, mail: Rc<Mail>, is_op: bool) {
        let imp = self.imp();

        // A rebound row (only possible past 205 messages) may still carry
        // the previous message's body, header card and actions; all three
        // belong to the fill.
        if imp.filled.get() {
            if let Some(view) = imp.view.get() {
                view.buffer().set_text("");
            }
            self.remove_header();
            imp.filled.set(false);
            imp.highlighted.set(false);
        }
        if imp.group.borrow_mut().take().is_some() {
            self.insert_action_group("mailview", None::<&gio::SimpleActionGroup>);
        }
        imp.is_op.set(is_op);
        // Counted once here so the reseed walk stays O(1) per row: bodies
        // run to megabytes and reseed is called on every row of every pass.
        imp.body_lines.set(mail.body.lines().count().max(1) as i32);
        *imp.mail.borrow_mut() = Some(mail);

        // Seed the row's height before any of its widgetry exists: the
        // header card estimate, one line-height per body line, and the
        // fixed chrome between them.
        imp.seed_key.set((0, 0));
        self.reseed();
    }

    /// Apply the current line-height and header-height estimates to the
    /// seeded height. Cheap no-op unless an estimate changed since the last
    /// seeding, so the fill walk calls it on every row of every pass.
    fn reseed(&self) {
        let imp = self.imp();
        let key = (body_line_height(), header_height(imp.is_op.get()));
        if imp.seed_key.replace(key) == key {
            return;
        }
        if imp.mail.borrow().is_none() {
            return;
        }
        let (unit, header) = key;
        let height = imp
            .body_lines
            .get()
            .max(1)
            .saturating_mul(unit)
            .saturating_add(header)
            .saturating_add(ROW_CHROME_HEIGHT);
        self.set_size_request(-1, height);
    }

    fn is_filled(&self) -> bool {
        self.imp().filled.get()
    }

    fn is_highlighted(&self) -> bool {
        self.imp().highlighted.get()
    }

    /// Whether this row currently holds `mail`. Compared by Rc identity, not
    /// value: the overview's jump re-finds its target row across recycling
    /// (only possible past 205 messages), where two distinct messages could
    /// share a subject and author but never the same allocation.
    fn holds(&self, mail: &Rc<Mail>) -> bool {
        self.imp()
            .mail
            .borrow()
            .as_ref()
            .is_some_and(|held| Rc::ptr_eq(held, mail))
    }

    /// Give the row its header card and body text (or, on rebind, take the
    /// stale ones back). Rows fill once, from the warmup chain, and keep
    /// their content: the seeded height (reseed) stands in exactly until
    /// then, so the fill shifts nothing.
    ///
    /// Highlighting is deliberately NOT part of the fill — it costs as much
    /// again and runs as its own chain step (apply_highlight) a frame later.
    fn set_filled(&self, filled: bool) {
        let imp = self.imp();
        if imp.filled.get() == filled {
            return;
        }
        let mail = imp.mail.borrow().clone();
        let Some(mail) = mail else { return };
        if filled {
            let view = imp.ensure_view().clone();
            view.buffer().set_text(&mail.body);

            // First body anywhere: learn the real line height so every seed
            // from here on is exact. Measured as the advance between a one-
            // and a two-line layout, which is precisely what stacked plain
            // lines occupy in the view.
            if BODY_LINE_HEIGHT.with(Cell::get) == 0 {
                let one = view.create_pango_layout(Some("Mg"));
                let two = view.create_pango_layout(Some("Mg\nMg"));
                let unit = two.pixel_size().1 - one.pixel_size().1;
                if unit > 0 {
                    BODY_LINE_HEIGHT.with(|cell| cell.set(unit));
                }
            }

            let composer = imp.composer.get().expect("MessageRow composer set");
            let list = imp.list.get().expect("MessageRow list set");
            let group = build_body_action_group(&mail, &view, composer, list);
            // Added here rather than in build_body_action_group because they
            // need the toast overlay, which only the row holds (weakly — it
            // may already be gone during page teardown; the items then hide
            // as action-missing).
            if let Some(overlay) = imp.overlay.upgrade() {
                let hub = imp.fav_hub.get().expect("MessageRow fav_hub set");
                for action in build_favorite_actions(&mail, list, &overlay, hub) {
                    group.add_action(&action);
                }
            }
            self.insert_action_group("mailview", Some(&group));
            imp.set_selection_actions_enabled(&group, view.buffer().has_selection());
            *imp.group.borrow_mut() = Some(group);

            self.fill_header(&mail);
        } else {
            if let Some(view) = imp.view.get() {
                view.buffer().set_text("");
            }
            self.remove_header();
        }
        imp.filled.set(filled);
        imp.highlighted.set(false);
    }

    /// Build and mount the message's header card, above the body. Part of
    /// the fill for the same reason the body is: GtkListView realizes every
    /// row of a small model, so cards may not exist before the warmup
    /// reaches their row.
    fn fill_header(&self, mail: &Rc<Mail>) {
        let imp = self.imp();
        self.remove_header();
        let Some(overlay) = imp.overlay.upgrade() else {
            return;
        };
        let composer = imp.composer.get().expect("MessageRow composer set");
        let is_op = imp.is_op.get();

        let header = build_header_list(mail, &overlay, composer);
        // Selectable header labels replace right-clicks with their own stock
        // menu, shadowing the row's; hand them the mail actions as an extra
        // section. The action names resolve against the "mailview" group the
        // fill just inserted on this row.
        add_label_extra_menus(header.upcast_ref(), &build_header_extra_menu());
        let content = imp
            .content
            .get()
            .expect("ensure_view built the content box");
        content.prepend(&header);

        // First card of this kind anywhere: learn its exact height so every
        // seed from here on is exact. Nothing in a card wraps, so its height
        // is width-independent and one out-of-band measure is right.
        let known = if is_op {
            HEADER_HEIGHT_OP.with(Cell::get)
        } else {
            HEADER_HEIGHT_REPLY.with(Cell::get)
        };
        if known == 0 {
            let measured = header.measure(gtk::Orientation::Vertical, -1).1;
            if measured > 0 {
                if is_op {
                    HEADER_HEIGHT_OP.with(|cell| cell.set(measured));
                } else {
                    HEADER_HEIGHT_REPLY.with(|cell| cell.set(measured));
                }
                self.reseed();
            }
        }

        *imp.header.borrow_mut() = Some(header);
    }

    fn remove_header(&self) {
        let imp = self.imp();
        if let Some(header) = imp.header.borrow_mut().take()
            && let Some(content) = imp.content.get()
        {
            content.remove(&header);
        }
    }

    /// Colorize a filled body: quote levels, diff lines, headers. Runs as
    /// its own fill-chain step so text and colors each get their own frame.
    fn apply_highlight(&self) {
        let imp = self.imp();
        if !imp.filled.get() || imp.highlighted.get() {
            return;
        }
        // A bounded slice of lines per call: giant bodies (multi-MB patches
        // carry a tag span on nearly every line) would otherwise freeze a
        // whole frame — 946ms measured for a 7.9MB message. While slices
        // remain the row stays un-highlighted, so the fill chain keeps
        // calling back here.
        if let Some(view) = imp.view.get()
            && highlight::refresh_step(&view.buffer(), 1000)
        {
            return;
        }
        imp.highlighted.set(true);
    }
}

mod imp {
    use std::cell::{Cell, OnceCell, RefCell};
    use std::rc::Rc;

    use adw::prelude::*;
    use adw::subclass::prelude::*;
    use gtk::{gdk, gio, glib};

    use super::{Mail, build_body_menu};
    use crate::composer;
    use crate::highlight;

    #[derive(Default)]
    pub struct MessageObject {
        pub(super) mail: OnceCell<Rc<Mail>>,
        /// Whether this message heads its thread; see MessageObject::is_op.
        pub(super) is_op: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageObject {
        const NAME: &'static str = "KoshiMessageObject";
        type Type = super::MessageObject;
    }

    impl ObjectImpl for MessageObject {}

    #[derive(Default)]
    pub struct MessageRow {
        pub view: OnceCell<gtk::TextView>,
        pub popover: OnceCell<gtk::PopoverMenu>,
        pub group: RefCell<Option<gio::SimpleActionGroup>>,
        pub composer: OnceCell<composer::Composer>,
        /// The lore list slug the thread was opened from, keyed into
        /// favorites so they can be fetched again.
        pub(super) list: OnceCell<String>,
        /// The page's favorite hub, so this row's Add/Remove context actions
        /// stay in step with the header star (see FavoriteHub).
        pub(super) fav_hub: OnceCell<super::FavoriteHub>,
        /// The page's toast overlay, for the header card's address pills.
        /// Weak — it is an ancestor of this row.
        pub(super) overlay: glib::WeakRef<adw::ToastOverlay>,
        /// The column holding the header card over the body scroller.
        pub(super) content: OnceCell<gtk::Box>,
        /// The mounted header card, rebuilt by each fill.
        pub(super) header: RefCell<Option<gtk::ListBox>>,
        /// The currently bound message, filled into the buffer on demand.
        pub(super) mail: RefCell<Option<Rc<super::Mail>>>,
        /// Whether this row shows the thread's first message. Kept per object
        /// (not derived from position) because the single view can put a reply
        /// at position 0; it picks the OP/reply header-height seed slot.
        pub(super) is_op: Cell<bool>,
        /// The bound body's line count, counted once per bind.
        pub(super) body_lines: Cell<i32>,
        /// Whether the buffer currently holds the bound message's body.
        pub(super) filled: Cell<bool>,
        /// Whether the filled body has had its highlight pass.
        pub(super) highlighted: Cell<bool>,
        /// The (line-height, header-height) pair the current seed was
        /// computed with, so reseed is a cheap no-op while the estimates
        /// are unchanged.
        pub(super) seed_key: Cell<(i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MessageRow {
        const NAME: &'static str = "KoshiMessageRow";
        type Type = super::MessageRow;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for MessageRow {
        fn dispose(&self) {
            // The popover is parented to this row rather than to a child, so
            // it must be explicitly unparented before the row is finalized.
            if let Some(popover) = self.popover.get() {
                popover.unparent();
            }
        }
    }

    impl WidgetImpl for MessageRow {}
    impl BinImpl for MessageRow {}

    impl MessageRow {
        /// Build the message-independent widgetry on first use. GtkListView
        /// instantiates every row of a small model at once, so none of this
        /// — TextView, scroller, clamp, context menu — may exist until the
        /// row actually gets a message to show.
        pub(super) fn ensure_view(&self) -> &gtk::TextView {
            if let Some(view) = self.view.get() {
                return view;
            }
            let view = gtk::TextView::builder()
                .editable(false)
                .cursor_visible(false)
                .monospace(true)
                .wrap_mode(gtk::WrapMode::None)
                .left_margin(12)
                .right_margin(12)
                .top_margin(12)
                .bottom_margin(12)
                .build();

            // Bodies don't wrap (patches carry deliberately long lines), so a
            // horizontal-only scroller handles overflow; the natural height is
            // propagated so the list row is exactly as tall as the message.
            let hscroll = gtk::ScrolledWindow::builder()
                .child(&view)
                .hscrollbar_policy(gtk::PolicyType::Automatic)
                .vscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .build();

            // The column the fill mounts the header card into, above the
            // body. Its top margin and spacing are part of the row height
            // seed (ROW_CHROME_HEIGHT) — keep them in step.
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(12)
                .margin_top(12)
                .build();
            content.append(&hscroll);

            // The ListView is the ScrolledWindow's scrollable child (so it can
            // virtualize), which means the reading-width clamp lives per row
            // rather than around the whole stack.
            let clamp = adw::Clamp::builder()
                .maximum_size(1100)
                .tightening_threshold(800)
                .child(&content)
                .build();
            self.obj().set_child(Some(&clamp));
            self.content.set(content).ok();

            highlight::attach(&view.buffer());

            // The popover can't be parented to the TextView itself (it warns
            // about foreign children), so it hangs off the row.
            let popover = gtk::PopoverMenu::from_model(Some(&build_body_menu()));
            popover.set_parent(&*self.obj());
            popover.set_has_arrow(false);
            popover.set_halign(gtk::Align::Start);

            let gesture = gtk::GestureClick::new();
            gesture.set_button(gdk::BUTTON_SECONDARY);
            gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
            let popover_weak = popover.downgrade();
            gesture.connect_pressed(move |gesture, _, x, y| {
                let Some(popover) = popover_weak.upgrade() else {
                    return;
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            });
            view.add_controller(gesture);

            // The body gesture claims its clicks in the capture phase, and
            // selectable header labels claim theirs (their extra menu carries
            // the mail actions instead); this bubble-phase gesture catches
            // right-clicks on the rest of the card — header padding, gaps —
            // so the actions are reachable from anywhere on the message.
            let row_gesture = gtk::GestureClick::new();
            row_gesture.set_button(gdk::BUTTON_SECONDARY);
            let popover_weak = popover.downgrade();
            row_gesture.connect_pressed(move |gesture, _, x, y| {
                let Some(popover) = popover_weak.upgrade() else {
                    return;
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            });
            self.obj().add_controller(row_gesture);

            // Enable the selection-dependent actions on the currently bound
            // group whenever the selection changes. The group is rebuilt per
            // bind, so this looks it up by reference rather than capturing
            // specific action instances.
            let row_weak = self.obj().downgrade();
            view.buffer().connect_has_selection_notify(move |buffer| {
                if let Some(row) = row_weak.upgrade() {
                    let imp = row.imp();
                    if let Some(group) = imp.group.borrow().as_ref() {
                        imp.set_selection_actions_enabled(group, buffer.has_selection());
                    }
                }
            });

            self.view.set(view).ok();
            self.popover.set(popover).ok();
            self.view.get().expect("view just set")
        }

        pub fn set_selection_actions_enabled(&self, group: &gio::SimpleActionGroup, enabled: bool) {
            for name in ["quote-selection", "quote-with-date", "copy"] {
                if let Some(action) = group.lookup_action(name)
                    && let Ok(action) = action.downcast::<gio::SimpleAction>()
                {
                    action.set_enabled(enabled);
                }
            }
        }
    }
}

// The static context menu for a body view. Action names resolve against the
// "mailview" group rebuilt per message in build_body_action_group.
fn build_body_menu() -> gio::Menu {
    let quote_section = gio::Menu::new();
    quote_section.append(Some("_Quote Selection"), Some("mailview.quote-selection"));
    quote_section.append(Some("Quote With _Date"), Some("mailview.quote-with-date"));

    let edit_section = gio::Menu::new();
    edit_section.append(Some("_Copy"), Some("mailview.copy"));
    edit_section.append(Some("Select _All"), Some("mailview.select-all"));

    let mail_section = gio::Menu::new();
    mail_section.append(Some("_Reply"), Some("mailview.reply"));
    mail_section.append(Some("Open in New _Tab"), Some("mailview.open-new-tab"));
    mail_section.append(Some("Open on _Web"), Some("mailview.open-web"));
    mail_section.append(Some("View _Raw"), Some("mailview.raw"));

    let menu = gio::Menu::new();
    menu.append_section(None, &quote_section);
    menu.append_section(None, &edit_section);
    menu.append_section(None, &build_favorite_section());
    menu.append_section(None, &mail_section);
    menu
}

// Build the "mailview" action group for one message: selection quoting/copy
// plus the reply/open-web/raw mail actions. Rebuilt on every bind (visible
// rows only), which keeps build_mail_actions unchanged.
fn build_body_action_group(
    mail: &Mail,
    view: &gtk::TextView,
    composer: &composer::Composer,
    list: &str,
) -> gio::SimpleActionGroup {
    let insert_quoted = |prefix: Option<String>| {
        glib::clone!(
            #[weak]
            view,
            #[strong]
            composer,
            move |_: &gio::SimpleAction, _: Option<&glib::Variant>| {
                let buffer = view.buffer();
                if let Some((start, end)) = buffer.selection_bounds() {
                    let text = buffer.text(&start, &end, false);
                    let quoted: Vec<String> =
                        text.lines().map(|line| format!("> {line}")).collect();
                    let mut result = quoted.join("\n");
                    if let Some(prefix) = &prefix {
                        result = format!("{prefix}\n{result}");
                    }
                    composer.insert_quote(&result);
                }
            }
        )
    };

    let quote = gio::SimpleAction::new("quote-selection", None);
    quote.set_enabled(false);
    quote.connect_activate(insert_quoted(None));

    let quote_with_date = gio::SimpleAction::new("quote-with-date", None);
    quote_with_date.set_enabled(false);
    quote_with_date.connect_activate(insert_quoted(Some(format!(
        "On {}, {} wrote:",
        mail.date, mail.from
    ))));

    let copy = gio::SimpleAction::new("copy", None);
    copy.set_enabled(false);
    copy.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| {
            view.buffer().copy_clipboard(&view.clipboard());
        }
    ));

    let select_all = gio::SimpleAction::new("select-all", None);
    select_all.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| {
            let buffer = view.buffer();
            buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
        }
    ));

    let group = gio::SimpleActionGroup::new();
    group.add_action(&quote);
    group.add_action(&quote_with_date);
    group.add_action(&copy);
    group.add_action(&select_all);
    for action in build_mail_actions(mail, view.upcast_ref(), composer, list) {
        group.add_action(&action);
    }
    group
}

/// One step of warming the thread: give one more row its body text or its
/// highlight colors, nearest to the viewport first, until every realized
/// row is done. Rows are never emptied again — a freshly opened thread
/// warms completely within a few seconds of idle time, and from then on
/// scrolling costs nothing, anywhere. (Filling on demand instead was tried
/// and stutters: GtkListView parks rows without geometry right outside the
/// viewport, so "prefetch margins" cannot see them and every message
/// boundary crossed while scrolling paid its fill right in the hot path.)
///
/// One action per call: the work runs from an idle chain, never inside the
/// scroll machinery, and text and colors land in separate frames so neither
/// step alone blows the frame budget.
///
/// `viewport_ready` reports whether every row currently intersecting the
/// viewport is filled and highlighted — the reveal cover waits for that,
/// not for the whole warmup.
fn message_fill_step(
    list_view: &gtk::ListView,
    scrolled: &gtk::ScrolledWindow,
    viewport_ready: &Cell<bool>,
) -> bool {
    let viewport_height = scrolled.height() as f32;
    if viewport_height <= 0.0 {
        return false;
    }
    let mut fill: Option<(f32, MessageRow)> = None;
    let mut paint: Option<(f32, MessageRow)> = None;
    let mut ready = true;
    let mut child = list_view.first_child();
    while let Some(item_widget) = child {
        child = item_widget.next_sibling();
        let Some(row) = item_widget.first_child().and_downcast::<MessageRow>() else {
            continue;
        };
        // Keep every seed in step with the measured line height; exact
        // seeds mean a fill does not move the rows below it.
        row.reseed();
        // GtkListView gives real geometry only to the tiles around the
        // viewport; the rest of its realized children are parked with
        // child-visible unset and stale bounds. Parked rows still have live
        // widgets — warm them, just after everything with a known position.
        let distance = if !item_widget.is_child_visible() {
            f32::INFINITY
        } else if let Some(bounds) = item_widget.compute_bounds(scrolled) {
            // Zero-height bounds mean the row hasn't been laid out yet;
            // its distance is unknown, not zero.
            if bounds.height() <= 0.0 {
                f32::INFINITY
            } else if bounds.y() > viewport_height {
                bounds.y() - viewport_height
            } else if bounds.y() + bounds.height() < 0.0 {
                -(bounds.y() + bounds.height())
            } else {
                0.0
            }
        } else {
            f32::INFINITY
        };
        if !row.is_filled() {
            if distance == 0.0 {
                ready = false;
            }
            if fill.as_ref().is_none_or(|(nearest, _)| distance < *nearest) {
                fill = Some((distance, row));
            }
        } else if !row.is_highlighted() {
            if distance == 0.0 {
                ready = false;
            }
            if paint
                .as_ref()
                .is_none_or(|(nearest, _)| distance < *nearest)
            {
                paint = Some((distance, row));
            }
        }
    }
    viewport_ready.set(ready);
    // Whichever pending action is nearest goes first, so a visible row gets
    // its colors before the warmup wanders off filling distant text.
    match (fill, paint) {
        (Some((fill_at, row)), Some((paint_at, paint_row))) => {
            if paint_at < fill_at {
                paint_row.apply_highlight();
            } else {
                row.set_filled(true);
            }
            true
        }
        (Some((_, row)), None) => {
            row.set_filled(true);
            true
        }
        (None, Some((_, row))) => {
            row.apply_highlight();
            true
        }
        (None, None) => false,
    }
}

/// Parse an mboxrd thread into its messages, ordered as lore's web view shows
/// them: depth-first over the reply graph. lore serves `t.mbox.gz` in
/// chronological order, not thread order — a reply written months after the
/// message it answers sits at the end of the file, nowhere near it — so the
/// messages are re-threaded here, once, and the message list, the overview
/// sidebar and the opened-message lookup all share the one order.
///
/// mboxrd ">From " escaping is undone on the raw message text before MIME
/// parsing: the mbox writer escapes raw file lines, so unescaping must happen
/// before any Content-Transfer-Encoding decoding, not after.
///
/// The whole pipeline works on bytes: messages carry their own charsets, and
/// only mailparse — which reads each part's declaration — may turn them into
/// text. A premature whole-file UTF-8 conversion would replace every KOI8-R
/// byte with U+FFFD before the parser could decode it.
fn parse_thread(mbox: &[u8]) -> Vec<Mail> {
    let mails: Vec<Mail> = split_mbox(mbox)
        .iter()
        .map(|raw| parse_message(&unescape_mboxrd(raw)))
        .collect();
    in_thread_order(mails)
}

/// Reorder a thread into the overview's depth-first display order, so the OP
/// heads the list and every reply follows the message it answers.
///
/// `thread_tree` is idempotent over this: the walk keeps each parent's
/// children in their existing relative order, so re-running it on the result
/// yields the same rows, now indexed in position order.
fn in_thread_order(thread: Vec<Mail>) -> Vec<Mail> {
    let order: Vec<usize> = thread_tree(&thread).iter().map(|row| row.index).collect();
    // Every message is emitted exactly once, so this is a total permutation
    // and no message can be dropped by the take().
    let mut mails: Vec<Option<Mail>> = thread.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| mails[index].take())
        .collect()
}

/// Split lines of a byte buffer, normalizing "\r\n" and a missing final
/// newline away (like str::lines does for text).
fn split_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|&b| b == b'\n').map(|line| {
        line.strip_suffix(b"\r\n")
            .or_else(|| line.strip_suffix(b"\n"))
            .unwrap_or(line)
    })
}

/// Split an mboxrd file into raw messages on its "From " separator lines.
/// Body lines starting with "From " are ">"-escaped in mboxrd, so a line
/// beginning "From " at column zero is always a separator. The separator
/// lines themselves are dropped; anything before the first one is too.
fn split_mbox(mbox: &[u8]) -> Vec<Vec<u8>> {
    let mut messages: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    for line in split_lines(mbox) {
        if line.starts_with(b"From ") {
            messages.extend(current.take());
            current = Some(Vec::new());
        } else if let Some(message) = current.as_mut() {
            message.extend_from_slice(line);
            message.push(b'\n');
        }
    }
    messages.extend(current);
    messages
}

/// Undo mboxrd body escaping: any line of one-or-more '>' followed by
/// "From " loses one leading '>'.
fn unescape_mboxrd(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in split_lines(body) {
        let arrows = line.iter().take_while(|&&b| b == b'>').count();
        if arrows > 0 && line[arrows..].starts_with(b"From ") {
            out.extend_from_slice(&line[1..]);
        } else {
            out.extend_from_slice(line);
        }
        out.push(b'\n');
    }
    if !body.ends_with(b"\n") {
        out.pop();
    }
    out
}

fn parse_message(raw: &[u8]) -> Mail {
    let unknown = || "(unknown)".to_string();
    // The raw view is best-effort text; unlike the body it has no charset
    // machinery, so lossy UTF-8 is the honest rendering of the raw bytes.
    let raw_text = || String::from_utf8_lossy(raw).into_owned();
    let Ok(parsed) = mailparse::parse_mail(raw) else {
        return Mail {
            subject: unknown(),
            from: unknown(),
            to: unknown(),
            to_addrs: Vec::new(),
            cc: None,
            cc_addrs: Vec::new(),
            date: unknown(),
            message_id: None,
            in_reply_to: None,
            body: String::new(),
            raw: raw_text(),
        };
    };

    let header = |name: &str| parsed.headers.get_first_value(name);
    let addrs = |name: &str| {
        parsed
            .headers
            .get_first_header(name)
            .map(parse_addresses)
            .unwrap_or_default()
    };
    let body = find_text_body(&parsed).unwrap_or_default();

    Mail {
        subject: header("Subject").unwrap_or_else(unknown),
        from: header("From").unwrap_or_else(unknown),
        to: header("To").unwrap_or_else(unknown),
        to_addrs: addrs("To"),
        cc: header("Cc"),
        cc_addrs: addrs("Cc"),
        date: header("Date").unwrap_or_else(unknown),
        message_id: header("Message-ID"),
        in_reply_to: header("In-Reply-To"),
        body: format!("{}\n", body.trim_end()),
        raw: raw_text(),
    }
}

/// Parse an address header into clean "Name <addr>" strings. RFC 5322
/// comments are stripped, and group syntax is flattened to its members.
fn parse_addresses(header: &mailparse::MailHeader) -> Vec<String> {
    let list = mailparse::addrparse_header(header).or_else(|_| {
        // mailparse chokes on nested comments (e.g. MAINTAINERS-style
        // "(open list:KERNEL HARDENING (not covered...))" entries), so strip
        // comments ourselves and retry.
        mailparse::addrparse(&strip_rfc5322_comments(&header.get_value()))
    });

    let Ok(list) = list else {
        // Last resort: crude comma split, keeping only address-shaped tokens.
        return header
            .get_value()
            .split(',')
            .map(str::trim)
            .filter(|token| token.contains('@'))
            .map(str::to_string)
            .collect();
    };

    let format_single = |info: &mailparse::SingleInfo| match &info.display_name {
        Some(name) if !name.is_empty() => format!("{} <{}>", name, info.addr),
        _ => info.addr.clone(),
    };

    list.iter()
        .flat_map(|addr| match addr {
            mailparse::MailAddr::Single(info) => vec![format_single(info)],
            mailparse::MailAddr::Group(group) => group.addrs.iter().map(format_single).collect(),
        })
        .collect()
}

/// Remove RFC 5322 comments, handling nesting, quoted strings and escapes.
fn strip_rfc5322_comments(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    let mut in_quotes = false;
    let mut escape = false;
    for c in s.chars() {
        if escape {
            if depth == 0 {
                out.push(c);
            }
            escape = false;
            continue;
        }
        match c {
            '\\' => {
                escape = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                in_quotes = !in_quotes;
                out.push(c);
            }
            '(' if !in_quotes => depth += 1,
            ')' if !in_quotes && depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

fn find_text_body(part: &mailparse::ParsedMail) -> Option<String> {
    if part.subparts.is_empty() {
        if part.ctype.mimetype.eq_ignore_ascii_case("text/plain") {
            let body = part.get_body().ok()?;
            // Senders occasionally declare a legacy charset on what is
            // really UTF-8 text. Legacy Cyrillic bytes essentially never
            // form valid multi-byte UTF-8 sequences, so when the decoded
            // bytes do validate as UTF-8 with non-ASCII content, believe
            // the bytes over the declaration.
            if !part.ctype.charset.eq_ignore_ascii_case("utf-8")
                && !body.is_ascii()
                && let Ok(bytes) = part.get_body_raw()
                && let Ok(utf8) = String::from_utf8(bytes)
                && !utf8.is_ascii()
            {
                return Some(utf8);
            }
            return Some(body);
        }
        return None;
    }
    part.subparts.iter().find_map(find_text_body)
}

pub const THREAD_PAGE_NAME: &str = "koshi-thread-page";

pub fn build_thread_page(
    nav: &adw::NavigationView,
    list: &str,
    message_id: &str,
) -> adw::NavigationPage {
    let remote = RemoteContent::new();
    // The overview sidebar arrives with the thread; until then the split
    // view has no sidebar and toggle_overview is a no-op.
    //
    // The split view stays collapsed permanently: side-by-side mode resizes
    // the message stack on every frame of the show/hide animation, and
    // re-laying-out all the TextViews and wrapped labels per frame stutters
    // badly (measured as runs of >100ms frames), while the collapsed
    // overlay slides over the unchanged content at full frame rate.
    let split = adw::OverlaySplitView::builder()
        .content(remote.widget())
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        .collapsed(true)
        .min_sidebar_width(360.0)
        .build();

    // A collapsed sidebar ignores sidebar-width-fraction and sizes to
    // max-sidebar-width, so track the allocated width and keep the max at
    // two thirds of it. A tick callback sees every size change (allocation
    // only moves during frame-clock frames) and the compare makes idle
    // frames free.
    let last_width = std::cell::Cell::new(0);
    split.add_tick_callback(move |split, _| {
        let width = split.width();
        if width != last_width.replace(width) {
            split.set_max_sidebar_width(f64::from(width) * 0.66);
        }
        glib::ControlFlow::Continue
    });

    let page = adw::NavigationPage::new(&split, "Loading…");
    page.set_widget_name(THREAD_PAGE_NAME);
    spawn_thread_load(
        remote,
        split,
        nav.clone(),
        page.clone(),
        list.to_string(),
        message_id.to_string(),
    );
    page
}

/// Flip the thread-overview sidebar of a thread page built by
/// build_thread_page. Does nothing until the thread has loaded (there is no
/// tree to show before that) or on pages that aren't thread pages.
pub fn toggle_overview(page: &adw::NavigationPage) {
    let Some(split) = page.child().and_downcast::<adw::OverlaySplitView>() else {
        return;
    };
    if split.sidebar().is_some() {
        split.set_show_sidebar(!split.shows_sidebar());
    }
}

/// Reveal and focus the find-in-thread bar of a thread page built by
/// build_thread_page. A no-op until the thread has loaded (the bar lives
/// inside the built content) or on pages that aren't thread pages — matching
/// toggle_overview.
pub fn start_thread_search(page: &adw::NavigationPage) {
    let Some(split) = page.child().and_downcast::<adw::OverlaySplitView>() else {
        return;
    };
    let Some(content) = split.content() else {
        return;
    };
    let Some(bar) = find_descendant::<gtk::SearchBar>(&content) else {
        return;
    };
    bar.set_search_mode(true);
    if let Some(entry) = find_descendant::<gtk::SearchEntry>(&bar) {
        entry.grab_focus();
        // Select any existing query so the next keystroke replaces it, like a
        // second Ctrl+F in an editor.
        entry.select_region(0, -1);
    }
}

/// First descendant of `root` that is a `T`, depth-first. Used to reach the
/// find bar (and its entry) from the thread page without threading a handle
/// back out through the async content build.
fn find_descendant<T: glib::prelude::IsA<gtk::Widget>>(root: &impl IsA<gtk::Widget>) -> Option<T> {
    let mut child = root.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(found) = widget.clone().downcast::<T>() {
            return Some(found);
        }
        if let Some(found) = find_descendant::<T>(&widget) {
            return Some(found);
        }
    }
    None
}

fn spawn_thread_load(
    remote: RemoteContent,
    split: adw::OverlaySplitView,
    nav: adw::NavigationView,
    page: adw::NavigationPage,
    list: String,
    message_id: String,
) {
    remote.show_loading();
    let cancellable = remote.cancellable();
    glib::spawn_future_local(async move {
        match lore::fetch_thread_mbox(&list, &message_id, &cancellable).await {
            Ok(mbox) => {
                // Parsing a big thread (hundreds of MIME messages) takes long
                // enough to stall the loading spinner; do it off-thread.
                let thread = gio::spawn_blocking(move || parse_thread(&mbox))
                    .await
                    .unwrap_or_default();
                if thread.is_empty() {
                    let error = lore::Error::Parse("the thread has no messages".to_string());
                    show_thread_error(&remote, &error, &split, &nav, &page, list, message_id);
                } else {
                    page.set_title(&thread[0].subject);
                    // The single view opens on whichever message the thread
                    // was reached through; find it before the thread is moved
                    // into the content.
                    let opened = opened_message_index(&thread, &message_id);
                    // Mounted under RemoteContent's still-spinning cover; the
                    // content itself lifts it once the visible rows are
                    // filled and painted (see build_thread_content).
                    let content = build_thread_content(
                        thread,
                        &list,
                        opened,
                        remote.downgrade(),
                        &split,
                    );
                    remote.show_content_covered(&content);
                }
            }
            Err(error) if error.is_cancelled() => {}
            Err(error) => show_thread_error(&remote, &error, &split, &nav, &page, list, message_id),
        }
    });
}

fn show_thread_error(
    remote: &RemoteContent,
    error: &lore::Error,
    split: &adw::OverlaySplitView,
    nav: &adw::NavigationView,
    page: &adw::NavigationPage,
    list: String,
    message_id: String,
) {
    // A prior successful load may have left a sidebar on the split; drop it so
    // the error page (and any retry) never shows a stale tree.
    split.set_sidebar(None::<&gtk::Widget>);

    let weak = remote.downgrade();
    let split = split.downgrade();
    let nav = nav.downgrade();
    let page = page.downgrade();

    // "Open on Web" hands the reader off to lore's own thread view (which caps
    // itself at 1000 messages), the useful escape hatch when we bail out —
    // above all on the too-large threads Retry can never get past.
    let escaped = glib::Uri::escape_string(message_id.trim().trim_matches(['<', '>']), None, true);
    let web_url = format!("{}/{}/{}/", lore::BASE_URL, list, escaped);
    let overlay = remote.widget().downgrade();
    let open_web: Box<dyn Fn()> = Box::new(move || {
        if let Some(overlay) = overlay.upgrade() {
            launch_uri(&overlay, &web_url);
        }
    });

    remote.show_error(
        error,
        move || {
            let (Some(remote), Some(split), Some(nav), Some(page)) = (
                weak.upgrade(),
                split.upgrade(),
                nav.upgrade(),
                page.upgrade(),
            ) else {
                return;
            };
            spawn_thread_load(remote, split, nav, page, list.clone(), message_id.clone());
        },
        Some(("Open on Web", open_web)),
    );
}

fn build_thread_content(
    thread: Vec<Mail>,
    list: &str,
    opened: usize,
    remote: crate::remote_page::RemoteContentWeak,
    split: &adw::OverlaySplitView,
) -> gtk::Box {
    let op = &thread[0];
    // The single view opens on this message; the header star and Reply act on
    // it too, so clamp it into range up front.
    let opened = opened.min(thread.len().saturating_sub(1));

    let overlay = adw::ToastOverlay::new();
    // The composer opens targeting the OP; each mail's Reply button can
    // retarget it later.
    let composer = composer::build_composer(build_reply_context(op));

    // Favorites toggled through the header star and through a message's
    // context menu must agree; the hub keeps every view of a Message-ID in
    // step. It rides the page, so its listeners drop with it.
    let hub = FavoriteHub::default();

    // The header star and Reply act on the message in focus: the opened
    // message in single view, the thread's first message in threaded view.
    let favorite_of = |mail: &Mail| {
        mail.message_id.as_ref().map(|id| Favorite {
            message_id: id.clone(),
            subject: mail.subject.clone(),
            date: mail.date.clone(),
            list: list.to_string(),
        })
    };
    let opened_fav = favorite_of(&thread[opened]);
    let op_fav = favorite_of(op);
    let opened_reply = build_reply_context(&thread[opened]);
    let op_reply = build_reply_context(op);

    // Single view is the default, so both start on the opened message.
    let star_target = Rc::new(RefCell::new(opened_fav.clone()));
    let reply_target = Rc::new(RefCell::new(opened_reply.clone()));

    let star_button = new_star_button(false);
    let refresh_star: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        star_button,
        #[strong]
        star_target,
        move || match star_target.borrow().as_ref() {
            Some(fav) => {
                star_button.set_sensitive(true);
                apply_star_state(&star_button, favorites::is_favorite(&fav.message_id));
            }
            // No Message-ID to key a favorite by.
            None => {
                star_button.set_sensitive(false);
                apply_star_state(&star_button, false);
            }
        }
    ));
    refresh_star();
    star_button.connect_clicked(glib::clone!(
        #[weak]
        overlay,
        #[strong]
        star_target,
        #[strong]
        hub,
        move |_| {
            let Some(fav) = star_target.borrow().clone() else {
                return;
            };
            let added = !favorites::is_favorite(&fav.message_id);
            hub.toggle(fav);
            overlay.add_toast(adw::Toast::new(if added {
                "Added to Favorites"
            } else {
                "Removed from Favorites"
            }));
        }
    ));
    hub.subscribe(Rc::new(glib::clone!(
        #[strong]
        star_target,
        #[strong]
        refresh_star,
        move |changed: &str| {
            if star_target
                .borrow()
                .as_ref()
                .is_some_and(|fav| fav.message_id == changed)
            {
                refresh_star();
            }
        }
    )));

    let reply_button = gtk::Button::builder()
        .icon_name("mail-reply-sender-symbolic")
        .tooltip_text("Reply")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    reply_button.connect_clicked(glib::clone!(
        #[strong]
        composer,
        #[strong]
        reply_target,
        move |_| composer.start_reply(reply_target.borrow().clone())
    ));

    // Switches the message pane between the single opened message and the
    // whole thread; wired to swap the ListView's model once both are built.
    let view_toggle = build_view_toggle();
    // Move the star and Reply onto the right message when the view flips.
    view_toggle.connect_active_notify(glib::clone!(
        #[strong]
        star_target,
        #[strong]
        reply_target,
        #[strong]
        opened_fav,
        #[strong]
        op_fav,
        #[strong]
        opened_reply,
        #[strong]
        op_reply,
        #[strong]
        refresh_star,
        move |toggle| {
            let single = toggle.active() == 0;
            *star_target.borrow_mut() = if single {
                opened_fav.clone()
            } else {
                op_fav.clone()
            };
            *reply_target.borrow_mut() = if single {
                opened_reply.clone()
            } else {
                op_reply.clone()
            };
            refresh_star();
        }
    ));

    let title_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(18)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    // An empty hexpanding filler pushes the controls to the trailing edge now
    // that no subject label spans the row; Reply and the star sit just left of
    // the view toggle.
    title_row.append(&gtk::Box::builder().hexpand(true).build());
    title_row.append(&reply_button);
    title_row.append(&star_button);
    title_row.append(&view_toggle);
    // Match the bodies' reading width so the row lines up with them.
    let title_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&title_row)
        .build();

    // Message bodies render through a ListView, but the virtualization that
    // matters happens at the row level (update_message_fills): GtkListView
    // itself keeps every row of a <=205-item model realized and bound, so
    // for typical threads it recycles nothing. Rows are cheap shells; only
    // the ones near the viewport hold (and Pango-shape) their body text.
    let model = gio::ListStore::new::<MessageObject>();
    let factory = gtk::SignalListItemFactory::new();
    let list = list.to_string();
    factory.connect_setup(glib::clone!(
        #[strong]
        composer,
        #[strong]
        overlay,
        #[strong]
        list,
        #[strong]
        hub,
        move |_, item| {
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list item is a ListItem");
            // Reading rows, not interactive ones: no activatable hover
            // styling, no selection, and no row focus — a focusable row
            // grabs focus on any click inside it, which makes the ListView
            // scroll-snap that row into view. With the row out of the focus
            // chain, clicks land on the TextView directly and the scroll
            // position stays put.
            item.set_activatable(false);
            item.set_selectable(false);
            item.set_focusable(false);
            let row = MessageRow::new(&composer, &overlay, &list, &hub);
            item.set_child(Some(&row));
        }
    ));
    factory.connect_bind(move |_, item| {
        let item = item
            .downcast_ref::<gtk::ListItem>()
            .expect("list item is a ListItem");
        let message = item
            .item()
            .and_downcast::<MessageObject>()
            .expect("item is a MessageObject");
        let row = item
            .child()
            .and_downcast::<MessageRow>()
            .expect("child is a MessageRow");
        // The OP's header card differs (no Subject row, since the pinned
        // title already shows it), so the row must know whether it holds the
        // OP. The single view can put a reply at position 0, so this reads
        // the flag off the message, not the row's position.
        row.set_message(message.message(), message.is_op());
    });

    // Both views draw from the same message objects; only which of them the
    // ListView is pointed at differs. The threaded model holds every message
    // in thread order; the single model holds just the opened one.
    // Each message moves into an Rc once (no body clone); the objects share
    // those handles and so does the overview sidebar's reply tree, so nothing
    // re-parses or re-copies the thread to render it twice.
    let mails: Vec<Rc<Mail>> = thread.into_iter().map(Rc::new).collect();
    let objects: Vec<MessageObject> = mails
        .iter()
        .enumerate()
        .map(|(index, mail)| MessageObject::new(mail.clone(), index == 0))
        .collect();
    for object in &objects {
        model.append(object);
    }
    let single_model = gio::ListStore::new::<MessageObject>();
    single_model.append(&objects[opened]);

    // Opening a message shows that message alone (single view); the toggle
    // switches to the whole thread. The model swap is all it takes — the
    // factory, warmup chain and composer are shared across both.
    let selection = gtk::NoSelection::new(Some(single_model.clone()));
    let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list_view.set_single_click_activate(false);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list_view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    // A brief blank cover over the message pane on every view switch. The
    // model swap itself is instant, and when the opened message is the OP the
    // two views start out pixel-identical — without a visible beat the toggle
    // looks like it did nothing. Same overlay-above-content shape as
    // RemoteContent's loading cover, but bare: the beat is far below the
    // show-a-spinner threshold, so it reads as a page flicker, not a load.
    let switch_cover = adw::Bin::builder()
        .css_classes(["background"])
        .visible(false)
        .build();
    let pane = gtk::Overlay::builder().child(&scrolled).build();
    pane.add_overlay(&switch_cover);

    // Generation counter so a toggle during the cover's beat extends it
    // rather than letting the earlier timeout cut the new beat short.
    let switch_generation = Rc::new(Cell::new(0u32));
    view_toggle.connect_active_notify(glib::clone!(
        #[strong]
        model,
        #[strong]
        single_model,
        #[strong]
        switch_generation,
        #[weak]
        switch_cover,
        move |toggle| {
            // Toggle index 0 is Single, 1 is Threaded (see build_view_toggle).
            if toggle.active() == 0 {
                selection.set_model(Some(&single_model));
            } else {
                selection.set_model(Some(&model));
            }
            switch_cover.set_visible(true);
            let generation = switch_generation.get() + 1;
            switch_generation.set(generation);
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(200),
                glib::clone!(
                    #[strong]
                    switch_generation,
                    #[weak]
                    switch_cover,
                    move || {
                        if switch_generation.get() == generation {
                            switch_cover.set_visible(false);
                        }
                    }
                ),
            );
        }
    ));

    // The overview sidebar rides the split view the page was built with; once
    // it is set, the F9 shortcut and the header-bar button (toggle_overview)
    // come alive. It drives this list_view to jump and follows its scroll to
    // highlight, and its own view-switch handler must run after the model swap
    // above, so it is wired here — after the toggle and the scroller exist.
    split.set_sidebar(Some(&build_overview_sidebar(
        &mails,
        &list_view,
        &view_toggle,
        &scrolled,
        opened,
    )));

    // Background warmup. GtkListView keeps every row of a <=205-item model
    // realized (a hardcoded widget window), so the expensive part — body
    // text and highlighting — trickles in one row per idle, nearest to the
    // viewport first, until the whole thread is warm; after that scrolling
    // does no work at all. Filling from inside the adjustment signals would
    // mutate layout mid-allocation, so the signals only (re)start the chain.
    let chain_active = Rc::new(Cell::new(false));
    // Set once every row intersecting the viewport is filled and painted;
    // the reveal cover waits for it while the warmup continues behind.
    let quiescent = Rc::new(Cell::new(false));
    let start_chain = glib::clone!(
        #[strong]
        chain_active,
        #[strong]
        quiescent,
        #[weak]
        list_view,
        #[weak]
        scrolled,
        move || {
            if chain_active.replace(true) {
                return;
            }
            let viewport_ready = Cell::new(false);
            glib::idle_add_local(glib::clone!(
                #[strong]
                chain_active,
                #[strong]
                quiescent,
                #[weak_allow_none]
                list_view,
                #[weak_allow_none]
                scrolled,
                move || {
                    let (Some(list_view), Some(scrolled)) = (&list_view, &scrolled) else {
                        chain_active.set(false);
                        return glib::ControlFlow::Break;
                    };
                    let more = message_fill_step(list_view, scrolled, &viewport_ready);
                    if viewport_ready.get() {
                        quiescent.set(true);
                    }
                    if more {
                        glib::ControlFlow::Continue
                    } else {
                        chain_active.set(false);
                        glib::ControlFlow::Break
                    }
                }
            ));
        }
    );
    let vadjustment = scrolled.vadjustment();
    vadjustment.connect_value_changed(glib::clone!(
        #[strong]
        start_chain,
        move |_| start_chain()
    ));
    vadjustment.connect_changed(move |_| start_chain());

    // The find bar (Ctrl+F) is pinned under the title and reveals over the
    // list; it draws from the same shared message Rcs, so it needs no copy of
    // the thread.
    let search_bar = build_thread_search(&list_view, &view_toggle, Rc::new(mails.clone()), opened);

    // The title stays pinned above the scrolling list rather than scrolling
    // away with it, so the subject and view toggle stay reachable.
    let inner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    inner.append(&title_clamp);
    inner.append(&search_bar);
    inner.append(&pane);
    overlay.set_child(Some(&inner));

    // The composer floats over the bottom of the mail pane rather than living
    // below it in the layout. If it shared the layout, expanding it would
    // shrink the scroller's viewport, and GtkListView re-anchors on a resize
    // — so a partially-scrolled message would jump. Instead a fixed strip is
    // reserved at the bottom of the scroller for the collapsed bar (measured
    // once, when the bar first maps), and the expanded editor overlays the
    // bottom of the content. The viewport never changes size, so the scroll
    // position — the top line the reader is looking at — stays put.
    let composer_widget = composer.widget().clone();
    pane.add_overlay(&composer_widget);
    pane.set_measure_overlay(&composer_widget, false);

    let reserved = Cell::new(false);
    composer_widget.connect_map(glib::clone!(
        #[weak]
        scrolled,
        move |bar| {
            if reserved.replace(true) {
                return;
            }
            scrolled.set_margin_bottom(bar.measure(gtk::Orientation::Vertical, -1).1);
        }
    ));

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_box.append(&overlay);

    // This content is mounted underneath RemoteContent's loading cover, so
    // the rows can lay out, fill and paint while the spinner keeps spinning;
    // lift the cover once the fill chain reports the viewport quiescent,
    // with a frame cap bounding the wait. The remote handle is weak — the
    // content lives inside the RemoteContent tree, so a strong one would be
    // a reference cycle.
    let frames = Cell::new(0u32);
    scrolled.add_tick_callback(move |_, _| {
        frames.set(frames.get() + 1);
        if quiescent.get() || frames.get() >= 60 {
            if let Some(remote) = remote.upgrade() {
                remote.reveal();
            }
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });

    content_box
}

/// One row of the overview tree in depth-first display order.
struct TreeRow {
    /// Index of the message in the thread slice.
    index: usize,
    depth: usize,
    /// Row position (in this Vec, not message index) of the parent, so a
    /// subtree can be hidden when its parent is collapsed. None for the
    /// messages that start their own subthread.
    parent_row: Option<usize>,
    /// Whether this message has at least one reply of its own.
    has_children: bool,
}

/// Arrange the thread as lore.kernel.org's overview does: depth-first over
/// the In-Reply-To graph, children in arrival order. A message whose parent
/// is missing from the thread starts at depth zero; a parent may well appear
/// later in the mbox than its reply (lore serves cover letters after the
/// first patch), so linking is by id, not by file position.
fn thread_tree<M: std::borrow::Borrow<Mail>>(thread: &[M]) -> Vec<TreeRow> {
    let mut position: HashMap<&str, usize> = HashMap::new();
    for (index, mail) in thread.iter().enumerate() {
        if let Some(id) = &mail.borrow().message_id {
            let id = normalize_message_id(id);
            // An empty Message-ID must not become a key: any message with
            // an empty In-Reply-To would then "reply" to it.
            if !id.is_empty() {
                position.entry(id).or_insert(index);
            }
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); thread.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, mail) in thread.iter().enumerate() {
        let parent = mail
            .borrow()
            .in_reply_to
            .as_deref()
            .map(normalize_message_id)
            .and_then(|id| position.get(id).copied())
            .filter(|&parent| parent != index);
        match parent {
            Some(parent) => children[parent].push(index),
            None => roots.push(index),
        }
    }

    // A pending entry carries what a row needs before it is emitted; children
    // are pushed in reverse so they pop back into arrival order.
    struct Pending {
        index: usize,
        depth: usize,
        parent_row: Option<usize>,
    }

    let mut rows: Vec<TreeRow> = Vec::with_capacity(thread.len());
    let mut emitted = vec![false; thread.len()];
    let mut stack: Vec<Pending> = roots
        .iter()
        .rev()
        .map(|&index| Pending {
            index,
            depth: 0,
            parent_row: None,
        })
        .collect();
    loop {
        while let Some(pending) = stack.pop() {
            // The emitted guard makes reference cycles finite: a child that
            // was already written out is not descended into again.
            if std::mem::replace(&mut emitted[pending.index], true) {
                continue;
            }
            let row_pos = rows.len();
            let kids = &children[pending.index];
            let has_children = kids.iter().any(|&kid| !emitted[kid]);
            for &kid in kids.iter().rev() {
                stack.push(Pending {
                    index: kid,
                    depth: pending.depth + 1,
                    parent_row: Some(row_pos),
                });
            }
            rows.push(TreeRow {
                index: pending.index,
                depth: pending.depth,
                parent_row: pending.parent_row,
                has_children,
            });
        }
        // Messages caught in a reference cycle have no root to be reached
        // from; surface the first stranded one as a root and keep going.
        match emitted.iter().position(|&done| !done) {
            Some(index) => stack.push(Pending {
                index,
                depth: 0,
                parent_row: None,
            }),
            None => return rows,
        }
    }
}

/// The index of the message the thread was opened through, matched by
/// Message-ID. Falls back to the OP (index 0) when the id isn't found — the
/// `r` pseudo-list and stale favorites can resolve to a thread whose exact
/// message this no longer names.
fn opened_message_index(thread: &[Mail], message_id: &str) -> usize {
    let wanted = normalize_message_id(message_id);
    if wanted.is_empty() {
        return 0;
    }
    thread
        .iter()
        .position(|mail| {
            mail.message_id
                .as_deref()
                .is_some_and(|id| normalize_message_id(id) == wanted)
        })
        .unwrap_or(0)
}

/// The comparable core of a Message-ID or In-Reply-To header: the first
/// <...> content if any (In-Reply-To may carry several ids or trailing
/// comments), the trimmed text otherwise.
fn normalize_message_id(header: &str) -> &str {
    let bracketed = header
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(id, _)| id);
    bracketed.unwrap_or_else(|| header.trim())
}

/// The display-name part of a From header, falling back to the whole value.
fn author_name(from: &str) -> &str {
    let name = from
        .split('<')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"')
        .trim();
    if name.is_empty() { from.trim() } else { name }
}

/// The Date header as UTC "YYYY-MM-DD HH:MM", lore-style; unparsable dates
/// fall through verbatim.
fn overview_date(date: &str) -> String {
    mailparse::dateparse(date)
        .ok()
        // dateparse yields Ok(0) for text it can't parse at all; a real
        // epoch-zero Date header is broken enough to show verbatim too.
        .filter(|&ts| ts != 0)
        .and_then(|ts| glib::DateTime::from_unix_utc(ts).ok())
        .and_then(|dt| dt.format("%Y-%m-%d %H:%M").ok())
        .map(|formatted| formatted.to_string())
        .unwrap_or_else(|| date.trim().to_string())
}

/// The overview row's texts: the message's own subject as the title, with
/// the author and date beneath. Every row shows its subject — the tree's
/// connector lines carry the "this is a reply to that" relationship, so the
/// subject never has to be dropped to signal it.
fn overview_row_texts(mail: &Mail) -> (String, String) {
    let title = mail.subject.trim();
    let title = if title.is_empty() {
        "(no subject)"
    } else {
        title
    };
    (
        title.to_string(),
        format!(
            "{} · {}",
            author_name(&mail.from),
            overview_date(&mail.date)
        ),
    )
}

/// Horizontal indent added per reply level, in pixels.
const OVERVIEW_INDENT: i32 = 22;
/// Cap on drawn indentation, so a pathological reply chain can't push the
/// row text off the side of the sidebar.
const OVERVIEW_MAX_DEPTH: usize = 12;

/// Per-row bookkeeping for the overview list: which row this is a reply to,
/// and whether its own replies are currently shown.
struct OverviewRow {
    /// Row position of the parent, for hiding a collapsed subtree.
    parent_row: Option<usize>,
    /// Whether this row's own replies are shown.
    expanded: Cell<bool>,
    /// Whether this row is shown at all — false once any ancestor collapses.
    /// The list's filter reads this; recompute it, then invalidate_filter.
    shown: Cell<bool>,
}

/// A row shows only while every ancestor is expanded. Rows sit in depth-first
/// order, so a parent's state is settled before its children are reached and
/// one forward pass suffices.
fn refresh_overview_visibility(rows: &[OverviewRow]) {
    for row in rows {
        let shown = match row.parent_row {
            None => true,
            Some(parent) => rows[parent].shown.get() && rows[parent].expanded.get(),
        };
        row.shown.set(shown);
    }
}

/// Scroll the message list so the row at `position` starts at the top of
/// the viewport. GtkListView's scroll_to is the only primitive that can
/// reach a parked row (parked rows have no geometry to compute a target
/// from), but it only scrolls the minimum needed to bring the row into
/// view; the follow-up tick aligns the row's top edge once the ListView
/// has placed it. Exact row seeds mean nothing shifts underneath, so the
/// loop converges within a few frames; the frame cap and the generation
/// counter (a newer jump supersedes a running one) bound it anyway.
fn jump_to_message(list_view: &gtk::ListView, position: u32, generation: &Rc<Cell<u64>>) {
    let this_jump = generation.get().wrapping_add(1);
    generation.set(this_jump);
    list_view.scroll_to(position, gtk::ListScrollFlags::NONE, None);

    // The tick callback re-finds the row by its message each frame: rows
    // can be recycled (>205 messages), so holding the row widget itself
    // would risk aligning to a rebound row.
    let Some(target) = list_view
        .model()
        .and_then(|model| model.item(position))
        .and_downcast::<MessageObject>()
    else {
        return;
    };
    let mail = target.message();
    let generation = generation.clone();
    let steady = Cell::new(0u32);
    let frames = Cell::new(0u32);
    list_view.add_tick_callback(move |list_view, _| {
        if generation.get() != this_jump {
            return glib::ControlFlow::Break;
        }
        frames.set(frames.get() + 1);
        let Some(scrolled) = list_view
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
        else {
            return glib::ControlFlow::Break;
        };
        // None while scroll_to hasn't placed the row yet — keep waiting
        // within the frame budget.
        if let Some(offset) = message_row_offset(list_view, &mail) {
            let vadjustment = scrolled.vadjustment();
            let max = (vadjustment.upper() - vadjustment.page_size()).max(vadjustment.lower());
            let desired = (vadjustment.value() + offset).clamp(vadjustment.lower(), max);
            // "Done" is the clamped target holding steady, which also covers
            // the last messages, whose tops can never reach the viewport top.
            if (desired - vadjustment.value()).abs() < 0.5 {
                steady.set(steady.get() + 1);
                if steady.get() >= 3 {
                    return glib::ControlFlow::Break;
                }
            } else {
                steady.set(0);
                vadjustment.set_value(desired);
            }
        }
        if frames.get() >= 60 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// The vertical offset of `mail`'s row from the top of the ListView's
/// visible area, or None while the row is parked or not laid out. The
/// ListView is the scrollable itself, so bounds relative to it are viewport
/// coordinates: offset 0 means the row starts exactly at the top.
fn message_row_offset(list_view: &gtk::ListView, mail: &Rc<Mail>) -> Option<f64> {
    let mut child = list_view.first_child();
    while let Some(item_widget) = child {
        child = item_widget.next_sibling();
        let Some(row) = item_widget.first_child().and_downcast::<MessageRow>() else {
            continue;
        };
        if !row.holds(mail) {
            continue;
        }
        if !item_widget.is_child_visible() {
            return None;
        }
        let bounds = item_widget.compute_bounds(list_view)?;
        if bounds.height() <= 0.0 {
            return None;
        }
        return Some(f64::from(bounds.y()));
    }
    None
}

/// The index (into `mails`) of the message whose row occupies the top of
/// the message viewport — the one the reader is looking at. None while no
/// realized row straddles the top (between frames, or mid-relayout right
/// after a model swap), so the caller keeps the current highlight rather
/// than clearing it. Only rows near the viewport are realized, so the walk
/// is over a handful of widgets.
fn message_at_viewport_top(list_view: &gtk::ListView, mails: &[Rc<Mail>]) -> Option<usize> {
    let mut fallback: Option<(f64, usize)> = None;
    let mut child = list_view.first_child();
    while let Some(item_widget) = child {
        child = item_widget.next_sibling();
        let Some(row) = item_widget.first_child().and_downcast::<MessageRow>() else {
            continue;
        };
        if !item_widget.is_child_visible() {
            continue;
        }
        let Some(bounds) = item_widget.compute_bounds(list_view) else {
            continue;
        };
        let (y, height) = (f64::from(bounds.y()), f64::from(bounds.height()));
        if height <= 0.0 {
            continue;
        }
        let Some(index) = mails.iter().position(|mail| row.holds(mail)) else {
            continue;
        };
        // The row spanning y = 0 is the one at the top of the viewport.
        if y <= 0.0 && y + height > 0.0 {
            return Some(index);
        }
        // No straddler yet (e.g. the frames right after a jump): fall back to
        // the closest row below the top edge.
        if y >= 0.0 && fallback.is_none_or(|(best, _)| y < best) {
            fallback = Some((y, index));
        }
    }
    fallback.map(|(_, index)| index)
}

/// One find-in-thread hit: a character range within message `msg`'s body.
/// Offsets index characters, so they map straight onto that body's buffer.
#[derive(Clone, Copy)]
struct SearchMatch {
    msg: usize,
    start: i32,
    end: i32,
}

/// Length-preserving lowercase fold: takes the first char of a char's Unicode
/// lowercasing. Nearly every mapping is one-to-one (Latin, Cyrillic, Greek),
/// so a match at folded index i is at character i of the original body; the
/// rare one-to-many folds (ß) are approximated rather than skewing offsets.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn fold_query(query: &str) -> Vec<char> {
    query.chars().map(fold_char).collect()
}

/// Character-offset ranges of every case-insensitive, non-overlapping
/// occurrence of `needle` (already folded by fold_query) in `body`.
fn body_matches(body: &str, needle: &[char]) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let hay: Vec<char> = body.chars().map(fold_char).collect();
    if needle.len() > hay.len() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == *needle {
            out.push((i as i32, (i + needle.len()) as i32));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// The realized MessageRow currently holding `mail`, parked or not — so search
/// can tag or clear its buffer even when the row is off-screen (its TextView
/// persists once built). For a <=205-message thread every row stays realized.
fn realized_message_row(list_view: &gtk::ListView, mail: &Rc<Mail>) -> Option<MessageRow> {
    let mut child = list_view.first_child();
    while let Some(item_widget) = child {
        child = item_widget.next_sibling();
        if let Some(row) = item_widget.first_child().and_downcast::<MessageRow>()
            && row.holds(mail)
        {
            return Some(row);
        }
    }
    None
}

/// The viewport y (0 = top of the visible area) of the match starting at
/// `offset` in `view`'s buffer, or None while the row isn't laid out. The
/// ListView is the scrollable, so coordinates relative to it are viewport
/// coordinates — the same frame jump_to_message nudges the vadjustment in.
fn match_viewport_y(list_view: &gtk::ListView, view: &gtk::TextView, offset: i32) -> Option<f64> {
    let iter = view.buffer().iter_at_offset(offset);
    let location = view.iter_location(&iter);
    let (_, widget_y) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, 0, location.y());
    let point = view.compute_point(list_view, &gtk::graphene::Point::new(0.0, widget_y as f32))?;
    Some(f64::from(point.y()))
}

/// Drop every realized row's search highlight. Only rows near the viewport
/// hold a built TextView, and past 205 messages rows recycle, so this walks
/// the realized set rather than the model.
fn clear_all_search_highlight(list_view: &gtk::ListView) {
    let mut child = list_view.first_child();
    while let Some(item_widget) = child {
        child = item_widget.next_sibling();
        if let Some(row) = item_widget.first_child().and_downcast::<MessageRow>()
            && let Some(view) = row.imp().view.get()
        {
            highlight::clear_search(&view.buffer());
        }
    }
}

/// Bring a search match into view: scroll_to the message's row (the only
/// primitive that reaches a parked row), fill it if the warmup hasn't yet,
/// paint its matches, then nudge the scroller so the match sits a little below
/// the top. Same tick-loop shape as jump_to_message — the generation counter
/// lets a newer match supersede a running scroll, and the frame cap bounds it.
fn scroll_to_match(
    list_view: &gtk::ListView,
    position: u32,
    mail: Rc<Mail>,
    offset: i32,
    ranges: Rc<Vec<(i32, i32)>>,
    current_local: usize,
    generation: &Rc<Cell<u64>>,
) {
    let this_jump = generation.get().wrapping_add(1);
    generation.set(this_jump);
    list_view.scroll_to(position, gtk::ListScrollFlags::NONE, None);

    let generation = generation.clone();
    let applied = Cell::new(false);
    let steady = Cell::new(0u32);
    let frames = Cell::new(0u32);
    list_view.add_tick_callback(move |list_view, _| {
        if generation.get() != this_jump {
            return glib::ControlFlow::Break;
        }
        frames.set(frames.get() + 1);
        let Some(scrolled) = list_view
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
        else {
            return glib::ControlFlow::Break;
        };
        if let Some(row) = realized_message_row(list_view, &mail) {
            // The warmup may not have reached this row; force its body in so
            // the match has text (and geometry) to scroll to.
            if !row.is_filled() {
                row.set_filled(true);
            }
            if let Some(view) = row.imp().view.get() {
                if !applied.replace(true) {
                    highlight::mark_search(&view.buffer(), &ranges, Some(current_local));
                }
                // None until the freshly filled row has laid out; keep waiting
                // within the frame budget.
                if let Some(y) = match_viewport_y(list_view, view, offset) {
                    let vadjustment = scrolled.vadjustment();
                    // Leave a margin so the match clears the pinned find bar
                    // and reads as "in context", not glued to the top edge.
                    let margin = 72.0;
                    let max =
                        (vadjustment.upper() - vadjustment.page_size()).max(vadjustment.lower());
                    let desired =
                        (vadjustment.value() + y - margin).clamp(vadjustment.lower(), max);
                    if (desired - vadjustment.value()).abs() < 0.5 {
                        steady.set(steady.get() + 1);
                        if steady.get() >= 3 {
                            return glib::ControlFlow::Break;
                        }
                    } else {
                        steady.set(0);
                        vadjustment.set_value(desired);
                    }
                }
            }
        }
        if frames.get() >= 90 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// The find-in-thread bar: a search entry with a match counter and prev/next
/// buttons. Revealed by Ctrl+F (start_thread_search); the buttons (reachable by
/// Tab) step through matches, Escape closes it.
///
/// Search follows the current view — only the opened message in single view,
/// the whole thread in threaded view — mirroring the view toggle rather than
/// carrying a scope control of its own. Matches are found in the raw body
/// strings (no need to have filled every row's TextView), so the counter is
/// exact; the hit itself is painted only in the message it lands in, when the
/// scroll fills that row.
fn build_thread_search(
    list_view: &gtk::ListView,
    view_toggle: &adw::ToggleGroup,
    mails: Rc<Vec<Rc<Mail>>>,
    opened: usize,
) -> gtk::SearchBar {
    let opened = opened.min(mails.len().saturating_sub(1));

    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Find in thread")
        .hexpand(true)
        .build();
    let count_label = gtk::Label::builder()
        .css_classes(["dim-label", "numeric"])
        .width_chars(10)
        .xalign(1.0)
        .build();

    let prev_button = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Previous Match")
        .build();
    let next_button = gtk::Button::builder()
        .icon_name("go-down-symbolic")
        .tooltip_text("Next Match")
        .build();
    let nav_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .build();
    nav_box.append(&prev_button);
    nav_box.append(&next_button);

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    row.append(&entry);
    row.append(&count_label);
    row.append(&nav_box);
    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&row)
        .build();
    let search_bar = gtk::SearchBar::builder().child(&clamp).build();
    search_bar.connect_entry(&entry);

    let matches: Rc<RefCell<Vec<SearchMatch>>> = Rc::new(RefCell::new(Vec::new()));
    let current: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    let anchor: Rc<Cell<usize>> = Rc::new(Cell::new(opened));
    let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    // Move to the match at `index`: flip to threaded view if the hit lives
    // outside the opened message, clear stale highlights, then paint and scroll
    // to it. Reads the target message's own matches so every hit in it lights
    // up, with the current one distinct.
    let select: Rc<dyn Fn(usize)> = Rc::new(glib::clone!(
        #[weak]
        list_view,
        #[weak]
        view_toggle,
        #[weak]
        count_label,
        #[strong]
        matches,
        #[strong]
        current,
        #[strong]
        mails,
        #[strong]
        generation,
        move |index: usize| {
            let matches_ref = matches.borrow();
            let Some(hit) = matches_ref.get(index).copied() else {
                return;
            };
            let total = matches_ref.len();
            let mut ranges = Vec::new();
            let mut current_local = 0;
            for other in matches_ref.iter().filter(|m| m.msg == hit.msg) {
                if other.start == hit.start && other.end == hit.end {
                    current_local = ranges.len();
                }
                ranges.push((other.start, other.end));
            }
            drop(matches_ref);

            current.set(Some(index));
            count_label.set_text(&format!("{} of {}", index + 1, total));

            // Search follows the current view: single view shows only the
            // opened message (position 0), threaded shows every message in
            // thread order, so a hit's position is its message index.
            let position = if view_toggle.active() == 0 {
                0
            } else {
                hit.msg as u32
            };

            clear_all_search_highlight(&list_view);
            scroll_to_match(
                &list_view,
                position,
                mails[hit.msg].clone(),
                hit.start,
                Rc::new(ranges),
                current_local,
                &generation,
            );
        }
    ));

    // Rebuild the match set for the current query and scope, refresh the
    // counter, and land on the nearest hit. Runs on every keystroke and on
    // scope/open changes.
    let recompute: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        entry,
        #[weak]
        list_view,
        #[weak]
        view_toggle,
        #[weak]
        count_label,
        #[weak]
        prev_button,
        #[weak]
        next_button,
        #[strong]
        matches,
        #[strong]
        current,
        #[strong]
        anchor,
        #[strong]
        mails,
        #[strong]
        select,
        move || {
            let query = entry.text().to_string();
            let needle = fold_query(&query);
            let single = view_toggle.active() == 0;

            // The landing anchor tracks where the reader is: the opened message
            // in single view, the message at the viewport top in threaded.
            let here = if single {
                opened
            } else {
                message_at_viewport_top(&list_view, &mails).unwrap_or_else(|| anchor.get())
            };
            anchor.set(here);

            // Search follows the current view: only the opened message in
            // single view, the whole thread in threaded view.
            let indices: Vec<usize> = if single {
                vec![opened]
            } else {
                (0..mails.len()).collect()
            };
            let mut found = Vec::new();
            if !needle.is_empty() {
                for &msg in &indices {
                    for (start, end) in body_matches(&mails[msg].body, &needle) {
                        found.push(SearchMatch { msg, start, end });
                    }
                }
            }
            let total = found.len();
            *matches.borrow_mut() = found;
            current.set(None);
            prev_button.set_sensitive(total > 0);
            next_button.set_sensitive(total > 0);

            if query.is_empty() {
                count_label.set_text("");
                clear_all_search_highlight(&list_view);
            } else if total == 0 {
                count_label.set_text("No results");
                clear_all_search_highlight(&list_view);
            } else {
                // Land on the first hit at or after the anchor message, so
                // find-as-you-type jumps to the nearest match ahead.
                let anchor_msg = anchor.get();
                let start_at = matches
                    .borrow()
                    .iter()
                    .position(|m| m.msg >= anchor_msg)
                    .unwrap_or(0);
                select(start_at);
            }
        }
    ));

    // Wrap-around step through the matches.
    let step: Rc<dyn Fn(i64)> = Rc::new(glib::clone!(
        #[strong]
        matches,
        #[strong]
        current,
        #[strong]
        select,
        move |delta: i64| {
            let total = matches.borrow().len() as i64;
            if total == 0 {
                return;
            }
            let from = current.get().map_or(0, |c| c as i64);
            let to = (from + delta).rem_euclid(total) as usize;
            select(to);
        }
    ));

    entry.connect_search_changed(glib::clone!(
        #[strong]
        recompute,
        move |_| recompute()
    ));
    // No Enter/Ctrl+G stepping on the entry: stepping is the two buttons,
    // reachable by Tab, so there is one obvious way to move between matches.
    next_button.connect_clicked(glib::clone!(
        #[strong]
        step,
        move |_| step(1)
    ));
    prev_button.connect_clicked(glib::clone!(
        #[strong]
        step,
        move |_| step(-1)
    ));

    // Search scope mirrors the view toggle, so re-run the search when the user
    // switches between single and threaded while the bar is open.
    view_toggle.connect_active_notify(glib::clone!(
        #[weak]
        search_bar,
        #[strong]
        recompute,
        move |_| {
            if search_bar.is_search_mode() {
                recompute();
            }
        }
    ));

    entry.connect_stop_search(glib::clone!(
        #[weak]
        search_bar,
        move |_| search_bar.set_search_mode(false)
    ));
    // Opening captures the current reading position as the anchor and searches
    // any leftover text; closing drops the highlight.
    search_bar.connect_search_mode_enabled_notify(glib::clone!(
        #[weak]
        list_view,
        #[strong]
        recompute,
        move |bar| {
            if bar.is_search_mode() {
                recompute();
            } else {
                clear_all_search_highlight(&list_view);
            }
        }
    ));

    search_bar
}

/// The overview sidebar: a heading over one activatable row per message,
/// laid out as a collapsible reply tree indented by depth. Activating a row
/// jumps the message list to that message; the disclosure button on a row
/// with replies hides or shows its subtree.
fn build_overview_sidebar(
    thread: &[Rc<Mail>],
    list_view: &gtk::ListView,
    view_toggle: &adw::ToggleGroup,
    scrolled: &gtk::ScrolledWindow,
    opened: usize,
) -> gtk::Widget {
    // Single selection is the highlight: the row of the message on screen is
    // selected, so the "navigation-sidebar" style marks it. Selecting a row
    // in code fires row-selected, not row-activated, so it never triggers a
    // jump of its own.
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();

    // The list rows are appended in tree order, so their list positions no
    // longer match message order; this maps row position -> message index.
    let mut message_of_row: Vec<usize> = Vec::with_capacity(thread.len());
    let mut rows: Vec<OverviewRow> = Vec::with_capacity(thread.len());
    // Disclosure buttons are wired in a second pass, once every row exists
    // to share; this keeps each button's row position alongside it.
    let mut disclosures: Vec<(usize, gtk::Button)> = Vec::new();

    for row in thread_tree(thread) {
        let mail = &thread[row.index];
        let (title, subtitle) = overview_row_texts(mail);

        let title_label = gtk::Label::builder()
            .label(&title)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .build();
        let subtitle_label = gtk::Label::builder()
            .label(&subtitle)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .build();
        let texts = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .spacing(2)
            // A few pixels past the row's own spacing so the text sits a
            // little clear of the avatar.
            .margin_start(4)
            .build();
        texts.append(&title_label);
        texts.append(&subtitle_label);

        let avatar = adw::Avatar::new(28, Some(author_name(&mail.from)), true);
        avatar.set_valign(gtk::Align::Center);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_end(6)
            // Reply depth is a leading margin, clamped so a runaway reply
            // chain can't push the text off the side.
            .margin_start(6 + row.depth.min(OVERVIEW_MAX_DEPTH) as i32 * OVERVIEW_INDENT)
            .build();

        // Every row reserves the same disclosure column so avatars line up
        // straight down the tree. A row with replies gets a live toggle; a
        // leaf gets the identical button kept invisible (opacity, not
        // visibility, so it still takes its exact width) and inert.
        let button = gtk::Button::builder()
            .icon_name("pan-down-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        if row.has_children {
            button.set_tooltip_text(Some("Collapse replies"));
        } else {
            button.set_opacity(0.0);
            button.set_can_focus(false);
            button.set_can_target(false);
            button.set_sensitive(false);
        }
        content.append(&button);

        content.append(&avatar);
        content.append(&texts);

        let row_widget = gtk::ListBoxRow::builder()
            .child(&content)
            .tooltip_text(&mail.subject)
            .build();
        list.append(&row_widget);

        let row_pos = rows.len();
        if row.has_children {
            disclosures.push((row_pos, button));
        }
        rows.push(OverviewRow {
            parent_row: row.parent_row,
            expanded: Cell::new(true),
            shown: Cell::new(true),
        });
        message_of_row.push(row.index);
    }

    let list_scrolled = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let rows = Rc::new(rows);

    // Collapsing hides a subtree by filtering it out. A filter (rather than
    // per-row set_visible) is the list's own mechanism for this: it drops the
    // rows from layout and repaints cleanly, leaving nothing behind.
    list.set_filter_func(glib::clone!(
        #[strong]
        rows,
        move |row| {
            rows.get(row.index() as usize)
                .is_none_or(|overview| overview.shown.get())
        }
    ));

    for (row_pos, button) in disclosures {
        button.connect_clicked(glib::clone!(
            #[strong]
            rows,
            #[weak]
            list,
            move |button| {
                let expanded = !rows[row_pos].expanded.get();
                rows[row_pos].expanded.set(expanded);
                button.set_icon_name(if expanded {
                    "pan-down-symbolic"
                } else {
                    "pan-end-symbolic"
                });
                button.set_tooltip_text(Some(if expanded {
                    "Collapse replies"
                } else {
                    "Expand replies"
                }));
                refresh_overview_visibility(&rows);
                list.invalidate_filter();
            }
        ));
    }

    // Message index -> the sidebar row that shows it, so the on-screen
    // message's row can be selected. Every message appears once in the tree.
    let mut row_of_message = vec![0i32; thread.len()];
    for (row_pos, &message) in message_of_row.iter().enumerate() {
        row_of_message[message] = row_pos as i32;
    }

    // The highlight marks the message the reader navigated to explicitly — the
    // opened message on entry, or the message an overview row jumped to — and
    // holds only until the reader scrolls the thread away from it. It is a fixed
    // marker, not a scroll-position tracker: the moment the reader scrolls, the
    // marked row no longer reflects what is on screen, so the marker is dropped
    // and the overview re-opens with nothing highlighted.
    let current: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(Some(opened)));
    // Suppresses the clear-on-scroll while a jump (and the view switch it rides
    // on) is still settling — both move the vadjustment on their own.
    let settling = Rc::new(Cell::new(false));
    // The vadjustment value the marker settled at; a later value more than a
    // pixel off is the reader scrolling away from the marked message.
    let anchor = Rc::new(Cell::new(0.0f64));
    // A generation counter so a new activation supersedes any settling loop
    // still running from a previous click.
    let scroll_generation = Rc::new(Cell::new(0u64));
    let row_of_message = Rc::new(row_of_message);

    let apply_highlight: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        list,
        #[strong]
        current,
        #[strong]
        row_of_message,
        move || {
            match current.get().and_then(|msg| row_of_message.get(msg).copied()) {
                Some(row_pos) => {
                    if let Some(row) = list.row_at_index(row_pos)
                        && !row.is_selected()
                    {
                        list.select_row(Some(&row));
                    }
                }
                None => list.unselect_all(),
            }
        }
    ));
    apply_highlight();

    list.connect_row_activated(glib::clone!(
        #[weak]
        list_view,
        #[weak]
        view_toggle,
        #[strong]
        scroll_generation,
        #[strong]
        current,
        #[strong]
        settling,
        #[strong]
        anchor,
        #[strong]
        apply_highlight,
        move |_, row| {
            let Some(&index) = message_of_row.get(row.index() as usize) else {
                return;
            };
            // Mark the picked message and raise the settling flag before moving
            // the view, so the view switch and jump below — which both nudge the
            // scroll — are not mistaken for the reader scrolling away.
            current.set(Some(index));
            settling.set(true);
            // The overview lists the whole thread, so a jump only lands
            // somewhere in the single view when it happens to be showing that
            // one message. Switch to the threaded view first (a no-op when
            // already there); its model holds every message in thread order,
            // so the message index is the row's list position.
            view_toggle.set_active(1);
            jump_to_message(&list_view, index as u32, &scroll_generation);
            apply_highlight();

            // Hold the marker on across the jump's settling frames, then anchor
            // it at the scroll position the jump came to rest on and re-arm the
            // clear-on-scroll. Gated by the jump generation so a newer click
            // takes over; the frame budget matches jump_to_message's.
            let generation = scroll_generation.clone();
            let this_jump = generation.get();
            let apply_highlight = apply_highlight.clone();
            let settling = settling.clone();
            let anchor = anchor.clone();
            let last = Cell::new(f64::NAN);
            let steady = Cell::new(0u32);
            let frames = Cell::new(0u32);
            list_view.add_tick_callback(move |list_view, _| {
                if generation.get() != this_jump {
                    // A newer jump owns the marker and runs its own settling.
                    return glib::ControlFlow::Break;
                }
                apply_highlight();
                let value = list_view
                    .ancestor(gtk::ScrolledWindow::static_type())
                    .and_downcast::<gtk::ScrolledWindow>()
                    .map_or(0.0, |scrolled| scrolled.vadjustment().value());
                frames.set(frames.get() + 1);
                if (value - last.get()).abs() < 0.5 {
                    steady.set(steady.get() + 1);
                } else {
                    steady.set(0);
                    last.set(value);
                }
                // Settle once the scroll holds steady, past an initial gate so
                // the pre-jump frames aren't taken for a settled position.
                if (frames.get() >= 4 && steady.get() >= 3) || frames.get() >= 60 {
                    anchor.set(value);
                    settling.set(false);
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });

            // The overview has done its job once a message is picked; dismiss
            // it so the message it jumps to is actually visible — open, it
            // overlays and dims most of the pane.
            if let Some(split) = row
                .ancestor(adw::OverlaySplitView::static_type())
                .and_downcast::<adw::OverlaySplitView>()
            {
                split.set_show_sidebar(false);
            }
        }
    ));

    // Scrolling the thread away from the marked message drops the marker; the
    // settling flag lets a jump's own scrolling through untouched.
    let vadjustment = scrolled.vadjustment();
    vadjustment.connect_value_changed(glib::clone!(
        #[strong]
        current,
        #[strong]
        settling,
        #[strong]
        anchor,
        #[strong]
        apply_highlight,
        move |vadjustment| {
            if settling.get() || current.get().is_none() {
                return;
            }
            if (vadjustment.value() - anchor.get()).abs() <= 1.0 {
                return;
            }
            current.set(None);
            apply_highlight();
        }
    ));
    // A manual view switch changes what is on screen, so it too drops the
    // marker; the settling flag exempts the switch the overview jump rides on.
    view_toggle.connect_active_notify(glib::clone!(
        #[strong]
        current,
        #[strong]
        settling,
        #[strong]
        apply_highlight,
        move |_| {
            if settling.get() || current.get().is_none() {
                return;
            }
            current.set(None);
            apply_highlight();
        }
    ));

    let heading = gtk::Label::builder()
        .label("Thread Overview")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let count = gtk::Label::builder()
        .label(format!(
            "{} message{}",
            thread.len(),
            if thread.len() == 1 { "" } else { "s" }
        ))
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    header.append(&heading);
    header.append(&count);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.append(&header);
    sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar.append(&list_scrolled);
    sidebar.upcast()
}

// The mail actions for the header card's selectable labels, resolved against
// the "mailview" group the fill inserts on the row.
fn build_header_extra_menu() -> gio::Menu {
    let mail_section = gio::Menu::new();
    mail_section.append(Some("_Reply"), Some("mailview.reply"));
    mail_section.append(Some("Open in New _Tab"), Some("mailview.open-new-tab"));
    mail_section.append(Some("Open on _Web"), Some("mailview.open-web"));
    mail_section.append(Some("View _Raw"), Some("mailview.raw"));

    let menu = gio::Menu::new();
    menu.append_section(None, &build_favorite_section());
    menu.append_section(None, &mail_section);
    menu
}

// Selectable labels pop their own stock menu on right-click, which would
// otherwise shadow the mail actions; append those there as an extra-menu
// section instead.
fn add_label_extra_menus(widget: &gtk::Widget, menu: &gio::Menu) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>()
        && label.is_selectable()
    {
        label.set_extra_menu(Some(menu));
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        add_label_extra_menus(&next, menu);
        child = next.next_sibling();
    }
}

/// Reply prefill: To = the author, Cc = everyone else on the thread,
/// Re:-prefixed subject and the mail's Message-ID for threading. Parsed
/// address lists are preferred; raw header values are the fallback.
fn build_reply_context(mail: &Mail) -> composer::ReplyContext {
    let mut cc: Vec<String> = Vec::new();
    if mail.to_addrs.is_empty() {
        cc.push(mail.to.clone());
    } else {
        cc.extend(mail.to_addrs.iter().cloned());
    }
    if mail.cc_addrs.is_empty() {
        cc.extend(mail.cc.clone());
    } else {
        cc.extend(mail.cc_addrs.iter().cloned());
    }

    composer::ReplyContext {
        to: mail.from.clone(),
        cc: cc.join(", "),
        subject: composer::reply_subject(&mail.subject),
        in_reply_to: mail.message_id.clone().unwrap_or_default(),
    }
}

/// A page-scoped hub that keeps every view of a mail's favorite state in
/// agreement: the header star button and each message's context-menu Add/
/// Remove actions. Toggling through any of them mutates the store and then
/// notifies the hub, so the others re-read the new state. It rides the page
/// (held by its subscribers in the widget tree) and drops with it, so its
/// listeners never accumulate across thread opens.
/// A view's callback: refresh yourself, the mail with this Message-ID just
/// had its favorite state toggled.
type FavoriteListener = Rc<dyn Fn(&str)>;

#[derive(Clone, Default)]
struct FavoriteHub {
    listeners: Rc<RefCell<Vec<FavoriteListener>>>,
}

impl FavoriteHub {
    fn subscribe(&self, listener: FavoriteListener) {
        self.listeners.borrow_mut().push(listener);
    }

    /// Flip `fav` in the store and tell every view of that Message-ID.
    fn toggle(&self, fav: Favorite) {
        let id = fav.message_id.clone();
        favorites::toggle(fav);
        self.notify(&id);
    }

    fn notify(&self, message_id: &str) {
        // Snapshot first: a listener could, in principle, subscribe another
        // while running, and mutating a borrowed Vec would panic.
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            listener(message_id);
        }
    }
}

/// The "mailview" favorite actions for one message: Add and Remove as a pair,
/// exactly one enabled at a time (their menu items hide via hidden-when, so
/// together they read as a single toggling entry). Both stay disabled when the
/// mail has no Message-ID to key the favorite by.
fn build_favorite_actions(
    mail: &Mail,
    list: &str,
    overlay: &adw::ToastOverlay,
    hub: &FavoriteHub,
) -> [gio::SimpleAction; 2] {
    let fav = mail.message_id.as_ref().map(|id| Favorite {
        message_id: id.clone(),
        subject: mail.subject.clone(),
        date: mail.date.clone(),
        list: list.to_string(),
    });

    let add = gio::SimpleAction::new("favorite-add", None);
    let remove = gio::SimpleAction::new("favorite-remove", None);

    // Both states derive from the store. `refresh` recomputes them, and the
    // hub calls it whenever this mail's favorite is toggled anywhere on the
    // page (this menu, or the header star), so the menu never goes stale.
    let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        add,
        #[weak]
        remove,
        #[strong]
        fav,
        move || {
            let starred = fav
                .as_ref()
                .is_some_and(|fav| favorites::is_favorite(&fav.message_id));
            add.set_enabled(fav.is_some() && !starred);
            remove.set_enabled(fav.is_some() && starred);
        }
    ));
    refresh();
    if let Some(fav) = &fav {
        let id = fav.message_id.clone();
        hub.subscribe(Rc::new(glib::clone!(
            #[strong]
            refresh,
            move |changed: &str| {
                if changed == id {
                    refresh();
                }
            }
        )));
    }

    // Each action drives the store toward its own named state (rather than
    // blindly flipping) and then notifies the hub, which resyncs this menu
    // and the header star.
    let activate = |target: bool| {
        glib::clone!(
            #[weak]
            overlay,
            #[strong]
            fav,
            #[strong]
            hub,
            move |_: &gio::SimpleAction, _: Option<&glib::Variant>| {
                let Some(fav) = &fav else { return };
                if favorites::is_favorite(&fav.message_id) != target {
                    hub.toggle(fav.clone());
                } else {
                    hub.notify(&fav.message_id);
                }
                overlay.add_toast(adw::Toast::new(if target {
                    "Added to Favorites"
                } else {
                    "Removed from Favorites"
                }));
            }
        )
    };
    add.connect_activate(activate(true));
    remove.connect_activate(activate(false));

    [add, remove]
}

/// The favorite pair as its own menu section, so it sits between separators
/// wherever it is appended. hidden-when makes the two items mutually
/// exclusive: whichever action is disabled disappears, so the section always
/// shows exactly one of Add/Remove (or neither, when the mail has no
/// Message-ID and both are disabled).
fn build_favorite_section() -> gio::Menu {
    let section = gio::Menu::new();
    let add = gio::MenuItem::new(Some("Add to _Favorites"), Some("mailview.favorite-add"));
    add.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    section.append_item(&add);
    let remove = gio::MenuItem::new(
        Some("Remove from _Favorites"),
        Some("mailview.favorite-remove"),
    );
    remove.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    section.append_item(&remove);
    section
}

/// A segmented switch between the single opened message and the whole thread.
/// Index 0 is Single (the default), index 1 is Threaded; the caller wires the
/// active-notify to swap the message model.
fn build_view_toggle() -> adw::ToggleGroup {
    let single = adw::Toggle::builder()
        .label("Single")
        .tooltip("Show only the opened message")
        .build();
    let threaded = adw::Toggle::builder()
        .label("Threaded")
        .tooltip("Show the whole thread")
        .build();

    let group = adw::ToggleGroup::builder()
        .valign(gtk::Align::Start)
        .build();
    group.add(single);
    group.add(threaded);
    group.set_active(0);
    group
}

/// A flat Reply icon button that retargets the composer to `mail`.
fn build_reply_button(mail: &Mail, composer: &composer::Composer) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("mail-reply-sender-symbolic")
        .tooltip_text("Reply")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let reply = build_reply_context(mail);
    button.connect_clicked(glib::clone!(
        #[strong]
        composer,
        move |_| composer.start_reply(reply.clone())
    ));
    button
}

/// A message's header card: subject, author and date over a collapsed
/// Details expander holding the noisier headers. Everything outside the
/// expander stays one line — values ellipsize rather than wrap — so every
/// card of a kind (OP or reply) is the same height and the row seeds stay
/// exact (see header_height).
fn build_header_list(
    mail: &Mail,
    overlay: &adw::ToastOverlay,
    composer: &composer::Composer,
) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["boxed-list"])
        .build();

    // Two title columns, each sized to the widest field name it holds so the
    // gap before the value is the same small margin in every row. Kept
    // separate — the always-visible rows never widen to fit the collapsed
    // Details names — so opening Details doesn't push the visible rows'
    // values across. These groups are built fresh with the card and thrown
    // away with it, so they sidestep the cross-card recycling hazard that
    // ruled a shared SizeGroup out (a persistent group re-negotiates every
    // card's layout as rows join and leave it on rebind).
    let visible_titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    let detail_titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);

    // The Subject row doubles as the card's toolbar: the Reply button trails
    // the hexpanding value label. Every card carries it now — the OP's too,
    // since the page no longer heads itself with the subject.
    let subject_row = build_single_line_row("Subject", &mail.subject, &visible_titles);
    subject_row.set_tooltip_text(Some(&mail.subject));
    if let Some(content) = subject_row.child().and_downcast::<gtk::Box>() {
        content.append(&build_reply_button(mail, composer));
    }
    list.append(&subject_row);

    list.append(&build_single_line_row(
        "Author",
        &mail.from,
        &visible_titles,
    ));
    list.append(&build_single_line_row("Date", &mail.date, &visible_titles));

    // The remaining headers are collapsed by default: recipients are almost
    // always the same as the OP's and the ids only matter for debugging, so
    // the Message-Id/In-Reply-To/To/Cc rows only show up on request.
    let details = adw::ExpanderRow::builder().title("Details").build();
    if let Some(id) = &mail.message_id {
        details.add_row(&build_text_row("Message-Id", id, &detail_titles));
    }
    if let Some(id) = &mail.in_reply_to {
        details.add_row(&build_text_row("In-Reply-To", id, &detail_titles));
    }
    details.add_row(&build_address_row(
        "To",
        &mail.to,
        &mail.to_addrs,
        overlay,
        &detail_titles,
    ));
    if let Some(cc) = &mail.cc {
        details.add_row(&build_address_row(
            "Cc",
            cc,
            &mail.cc_addrs,
            overlay,
            &detail_titles,
        ));
    }
    list.append(&details);

    list
}

/// A non-activatable row laying the field name and its value out on one
/// line: [title | value]. The title joins `title_group`, a per-card
/// SizeGroup that widens every member to its widest field name — so the
/// gap before the value is the box's fixed spacing, the same in every row
/// of the group, with no per-name slack. The always-visible rows and the
/// collapsed Details rows carry separate groups so opening Details never
/// shifts the visible values.
fn build_row(
    name: &str,
    value: &impl IsA<gtk::Widget>,
    title_valign: gtk::Align,
    title_group: &gtk::SizeGroup,
) -> gtk::ListBoxRow {
    // Top-aligned titles (wrapping chip rows) get nudged onto the first
    // value line; centered ones need no offset.
    let title_margin_top = if title_valign == gtk::Align::Start {
        6
    } else {
        0
    };
    let title = gtk::Label::builder()
        .label(name)
        .halign(gtk::Align::Start)
        .valign(title_valign)
        .margin_top(title_margin_top)
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    title_group.add_widget(&title);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .spacing(24)
        .build();
    content.append(&title);
    content.append(value);
    value.set_hexpand(true);

    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&content)
        .build()
}

fn build_text_row(name: &str, value: &str, title_group: &gtk::SizeGroup) -> gtk::ListBoxRow {
    // A wrapping label's natural width is far narrower than its full text,
    // so it must fill its allocation (halign Fill, the default) — with
    // halign Start it would shrink to that natural width and wrap long
    // before running out of row space. xalign keeps the text left-aligned.
    //
    // Plain stock selectable labels otherwise: GTK's own selection styling,
    // focus handling (including the text caret a click leaves), and context
    // menu. Forcing them non-focusable would kill the caret but also paints
    // every selection in the muted unfocused shade.
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(value)))
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    build_row(name, &label, gtk::Align::Center, title_group)
}

/// Like a text row, but the value never wraps: overlong values (subjects,
/// author display names, dates) ellipsize instead of growing the row, which
/// keeps every card of a kind the same height for the row seeds.
fn build_single_line_row(name: &str, value: &str, title_group: &gtk::SizeGroup) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(value)))
        .selectable(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    build_row(name, &label, gtk::Align::Center, title_group)
}

/// Address rows show parsed pills; if parsing produced nothing but the raw
/// header exists, fall back to a plain text row so the value isn't lost.
fn build_address_row(
    name: &str,
    raw: &str,
    addrs: &[String],
    overlay: &adw::ToastOverlay,
    title_group: &gtk::SizeGroup,
) -> gtk::ListBoxRow {
    if addrs.is_empty() {
        return build_text_row(name, raw, title_group);
    }

    let wrap = adw::WrapBox::builder()
        .child_spacing(6)
        .line_spacing(6)
        .build();
    for addr in addrs {
        wrap.append(&build_address_pill(addr, overlay));
    }

    let row = build_row(name, &wrap, gtk::Align::Start, title_group);
    // Nudge the title down so it baseline-aligns with the first chip line.
    if let Some(title) = wrap
        .parent()
        .and_downcast::<gtk::Box>()
        .and_then(|content| content.first_child())
    {
        title.set_margin_top(6);
    }
    row
}

fn build_address_pill(addr: &str, overlay: &adw::ToastOverlay) -> gtk::Button {
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(addr)))
        .css_classes(["caption"])
        .build();

    let button = gtk::Button::builder()
        .child(&label)
        .valign(gtk::Align::Center)
        .css_classes(["address-chip"])
        .build();

    let addr = addr.to_string();
    button.connect_clicked(glib::clone!(
        #[weak]
        overlay,
        move |button| {
            button.clipboard().set_text(&addr);
            overlay.add_toast(adw::Toast::new("Address copied"));
        }
    ));

    button
}

fn build_mail_actions(
    mail: &Mail,
    widget: &gtk::Widget,
    composer: &composer::Composer,
    list: &str,
) -> [gio::SimpleAction; 4] {
    let reply = gio::SimpleAction::new("reply", None);
    let reply_context = build_reply_context(mail);
    reply.connect_activate(glib::clone!(
        #[strong]
        composer,
        move |_, _| composer.start_reply(reply_context.clone())
    ));

    // The bare Message-ID (angle brackets stripped), shared by the lore URL
    // and the new-tab open, which both address a message by it.
    let bare_id = mail.message_id.as_deref().map(|id| {
        id.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string()
    });

    let open_web = gio::SimpleAction::new("open-web", None);
    let lore_url = bare_id
        .as_deref()
        .map(|bare| format!("https://lore.kernel.org/r/{bare}/"));
    open_web.set_enabled(lore_url.is_some());
    open_web.connect_activate(glib::clone!(
        #[weak]
        widget,
        move |_, _| {
            if let Some(url) = &lore_url {
                launch_uri(&widget, url);
            }
        }
    ));

    // Open this message on its own in a new tab: build_thread_page opens single
    // view on the given Message-ID, so the new tab lands showing just it.
    let open_new_tab = gio::SimpleAction::new("open-new-tab", None);
    open_new_tab.set_enabled(bare_id.is_some());
    let list = list.to_string();
    open_new_tab.connect_activate(glib::clone!(
        #[weak]
        widget,
        move |_, _| {
            let Some(bare) = &bare_id else { return };
            if let Some(tab_view) = widget
                .ancestor(adw::TabView::static_type())
                .and_downcast::<adw::TabView>()
            {
                crate::open_thread_in_new_tab(&tab_view, &list, bare);
            }
        }
    ));

    // View Raw opens in a new tab (selected, since it's an explicit "show me
    // this now"), so the message it was invoked from stays put in its own tab.
    let raw = gio::SimpleAction::new("raw", None);
    let raw_text = mail.raw.clone();
    let subject = mail.subject.clone();
    raw.connect_activate(glib::clone!(
        #[weak]
        widget,
        move |_, _| {
            if let Some(tab_view) = widget
                .ancestor(adw::TabView::static_type())
                .and_downcast::<adw::TabView>()
            {
                crate::open_raw_in_new_tab(&tab_view, &raw_text, &subject);
            }
        }
    ));

    [reply, open_web, open_new_tab, raw]
}

pub(crate) fn build_raw_page(raw: &str, subject: &str) -> adw::NavigationPage {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    view.buffer().set_text(raw);

    let scrolled = gtk::ScrolledWindow::builder().child(&view).build();
    adw::NavigationPage::new(&scrolled, &format!("Raw - {subject}"))
}

pub(crate) fn launch_uri(widget: &impl IsA<gtk::Widget>, uri: &str) {
    let parent = widget.root().and_downcast::<gtk::Window>();
    gtk::UriLauncher::new(uri).launch(parent.as_ref(), gio::Cancellable::NONE, |result| {
        if let Err(error) = result {
            eprintln!("Failed to launch URI: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real lore thread bundled as an offline fixture.
    const RAW_THREAD: &[u8] = include_bytes!("../data/sample-thread.mbox");

    #[test]
    fn parses_the_whole_thread() {
        let thread = parse_thread(RAW_THREAD);
        assert_eq!(thread.len(), 8);

        let op = &thread[0];
        assert_eq!(op.from, "Linus Walleij <linusw@kernel.org>");
        assert_eq!(
            op.subject,
            "[PATCH] mfd: db8500-prcmu: Fold dbx500 header into db8500"
        );
        assert_eq!(
            op.message_id.as_deref(),
            Some("<20260619-mfd-prcmu-merge-headers-v1-1-8ea0ee23b4d6@kernel.org>")
        );
        assert!(op.body.starts_with("Move the DBx500 PRCMU definitions"));

        // Every message keeps its own raw text (mbox From-line stripped)
        // and a non-empty parsed body normalized to exactly one trailing
        // newline.
        for mail in &thread {
            assert!(!mail.body.trim().is_empty(), "empty body for {}", mail.from);
            assert_eq!(
                mail.body,
                format!("{}\n", mail.body.trim_end()),
                "unnormalized body: {}",
                mail.from
            );
            assert!(
                !mail.raw.starts_with("From "),
                "mbox line kept: {}",
                mail.from
            );
            assert!(
                mail.raw.contains("Subject:"),
                "raw truncated: {}",
                mail.from
            );
        }

        // Quoted-printable reply bodies must come out decoded.
        assert_eq!(thread[1].from, "sashiko-bot@kernel.org");
        assert!(thread[1].body.contains("found 4 potential issue(s)"));
    }

    #[test]
    fn parses_op_address_lists_cleanly() {
        let thread = parse_thread(RAW_THREAD);
        let op = &thread[0];
        assert_eq!(op.to_addrs.len(), 17, "To: {:?}", op.to_addrs);
        assert_eq!(op.to_addrs[0], "Russell King <linux@armlinux.org.uk>");
        assert_eq!(op.cc_addrs.len(), 7, "Cc: {:?}", op.cc_addrs);
        assert!(
            op.cc_addrs
                .contains(&"kernel test robot <lkp@intel.com>".to_string())
        );
        assert!(
            op.cc_addrs
                .contains(&"linux-clk@vger.kernel.org".to_string())
        );
    }

    #[test]
    fn reply_context_targets_the_clicked_message() {
        let thread = parse_thread(RAW_THREAD);
        let reply = build_reply_context(&thread[1]);
        assert_eq!(reply.to, "sashiko-bot@kernel.org");
        assert!(reply.cc.contains("Linus Walleij <linusw@kernel.org>"));
        assert!(reply.cc.contains("linux-watchdog@vger.kernel.org"));
        assert_eq!(
            reply.subject,
            "Re: [PATCH] mfd: db8500-prcmu: Fold dbx500 header into db8500"
        );
        assert_eq!(
            reply.in_reply_to,
            "<20260619204041.040D71F000E9@smtp.kernel.org>"
        );
    }

    #[test]
    fn parses_in_reply_to_per_message() {
        let thread = parse_thread(RAW_THREAD);
        // The OP starts the thread, so it has no In-Reply-To.
        assert_eq!(thread[0].in_reply_to, None);
        // Every reply carries one; most point at the OP directly.
        for mail in &thread[1..] {
            assert!(mail.in_reply_to.is_some(), "no In-Reply-To: {}", mail.from);
        }
        assert_eq!(
            thread[1].in_reply_to.as_deref(),
            Some("<20260619-mfd-prcmu-merge-headers-v1-1-8ea0ee23b4d6@kernel.org>")
        );
    }

    #[test]
    fn splits_on_separator_lines_only() {
        let mbox = b"From a@b Thu Jan  1 00:00:00 1970\nSubject: x\n\n>From escaped\n\
                    From c@d Thu Jan  1 00:00:00 1970\nSubject: y\n\nbody\n";
        let messages = split_mbox(mbox);
        assert_eq!(messages.len(), 2);
        let first = String::from_utf8(messages[0].clone()).unwrap();
        let second = String::from_utf8(messages[1].clone()).unwrap();
        assert!(first.starts_with("Subject: x"));
        assert!(first.contains(">From escaped"));
        assert!(second.starts_with("Subject: y"));
    }

    #[test]
    fn unescapes_mboxrd_from_lines() {
        let body = b">From here\n>>From nested\n> From untouched\nno From here\n";
        assert_eq!(
            unescape_mboxrd(body),
            b"From here\n>From nested\n> From untouched\nno From here\n"
        );
        // Trailing-newline shape is preserved.
        assert_eq!(unescape_mboxrd(b">From x"), b"From x");
    }

    #[test]
    fn unescapes_raw_text_before_transfer_decoding() {
        // Writer-escaped ">From " must lose its '>' before qp decoding,
        // while a '>' the qp decoding itself produces ("=3EFrom") was never
        // escaped by the writer and must survive untouched.
        let raw = b"Subject: qp\nContent-Transfer-Encoding: quoted-printable\n\n\
                   >From escaped by the writer\n=3EFrom decoded, stays quoted\n";
        // Compared line-wise: mailparse emits CRLF for decoded qp bodies.
        let mail = parse_message(&unescape_mboxrd(raw));
        let mut lines = mail.body.lines();
        assert_eq!(lines.next(), Some("From escaped by the writer"));
        assert_eq!(lines.next(), Some(">From decoded, stays quoted"));
    }

    #[test]
    fn decodes_declared_legacy_charsets() {
        // "Привет" in KOI8-R — bytes that a premature whole-file UTF-8
        // conversion would have destroyed before the MIME parser could
        // read the charset declaration.
        let mut mbox = b"From x Thu Jan  1 00:00:00 1970\n\
                        From: x@y\n\
                        Subject: koi8\n\
                        Content-Type: text/plain; charset=koi8-r\n\
                        Content-Transfer-Encoding: 8bit\n\n"
            .to_vec();
        mbox.extend_from_slice(&[0xF0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4, b'\n']);
        let thread = parse_thread(&mbox);
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].body, "Привет\n");
    }

    #[test]
    fn believes_utf8_bytes_over_a_wrong_legacy_charset_label() {
        let mut mbox = b"From x Thu Jan  1 00:00:00 1970\n\
                        From: x@y\n\
                        Subject: mislabeled\n\
                        Content-Type: text/plain; charset=koi8-r\n\
                        Content-Transfer-Encoding: 8bit\n\n"
            .to_vec();
        mbox.extend_from_slice("Привет\n".as_bytes());
        let thread = parse_thread(&mbox);
        assert_eq!(thread[0].body, "Привет\n");
    }

    /// A minimal Mail for tree tests: only the fields the overview reads.
    fn mail(id: &str, in_reply_to: Option<&str>, subject: &str, from: &str) -> Mail {
        Mail {
            subject: subject.to_string(),
            from: from.to_string(),
            to: String::new(),
            to_addrs: Vec::new(),
            cc: None,
            cc_addrs: Vec::new(),
            date: "Mon, 29 Jun 2026 03:51:00 +0000".to_string(),
            message_id: Some(format!("<{id}>")),
            in_reply_to: in_reply_to.map(|id| format!("<{id}>")),
            body: String::new(),
            raw: String::new(),
        }
    }

    #[test]
    fn tree_nests_replies_depth_first() {
        // op ─ a ─ c ─ d, and op ─ b: DFS must visit a's subtree before b.
        let thread = [
            mail("op@x", None, "[PATCH 0/2] series", "Nika"),
            mail("a@x", Some("op@x"), "Re: [PATCH 0/2] series", "Miguel"),
            mail("b@x", Some("op@x"), "[PATCH 1/2] first", "Nika"),
            mail("c@x", Some("a@x"), "Re: [PATCH 0/2] series", "Nika"),
            mail("d@x", Some("c@x"), "Re: [PATCH 0/2] series", "Miguel"),
        ];
        let rows: Vec<(usize, usize)> = thread_tree(&thread)
            .iter()
            .map(|row| (row.index, row.depth))
            .collect();
        assert_eq!(rows, [(0, 0), (1, 1), (3, 2), (4, 3), (2, 1)]);
    }

    #[test]
    fn tree_roots_orphans_and_survives_cycles() {
        let thread = [
            // Replies to itself: must not recurse forever.
            mail("self@x", Some("self@x"), "loop", "A"),
            // Parent not in the thread: becomes a root.
            mail("orphan@x", Some("gone@x"), "orphan", "B"),
            // A mutual reference cycle: neither is reachable from a root.
            mail("early@x", Some("late@x"), "early", "C"),
            mail("late@x", Some("early@x"), "late", "D"),
        ];
        let rows = thread_tree(&thread);
        assert_eq!(rows.len(), thread.len());
        assert_eq!(
            rows.iter().filter(|row| row.depth == 0).count(),
            3,
            "self-reply, orphan and one cycle member are roots"
        );
        // The cycle is cut once: its first message roots it (row 2), the
        // other nests under it (parent_row is a row position, not an index).
        let late = rows.iter().find(|row| row.index == 3).unwrap();
        assert_eq!((late.parent_row, late.depth), (Some(2), 1));
    }

    #[test]
    fn tree_ignores_empty_ids() {
        // A bare "Message-ID:" header parses to Some(""); a bare
        // "In-Reply-To:" likewise. Neither may link the two messages.
        let mut a = mail("x@x", None, "a", "A");
        a.message_id = Some(String::new());
        let mut b = mail("y@y", None, "b", "B");
        b.in_reply_to = Some(String::new());
        let rows = thread_tree(&[a, b]);
        assert!(
            rows.iter()
                .all(|row| row.depth == 0 && row.parent_row.is_none())
        );
    }

    #[test]
    fn tree_resolves_parents_that_arrive_later() {
        // lore's t.mbox can serve a cover letter after the first patch; the
        // patches must still nest under it.
        let thread = [
            mail("p1@x", Some("cover@x"), "[PATCH 1/2] first", "Nika"),
            mail("cover@x", None, "[PATCH 0/2] series", "Nika"),
            mail("p2@x", Some("cover@x"), "[PATCH 2/2] second", "Nika"),
        ];
        let rows: Vec<(usize, usize)> = thread_tree(&thread)
            .iter()
            .map(|row| (row.index, row.depth))
            .collect();
        assert_eq!(rows, [(1, 0), (0, 1), (2, 1)]);
    }

    /// The shape that exposed the bug: lore serves `t.mbox.gz` by date, and
    /// this series (per-cpu work helpers v4) got its reviews in May and the
    /// author's answers to every one of them two months later, in one sitting.
    /// Chronologically those answers all pile up at the end of the file.
    fn late_replies_thread() -> Vec<Mail> {
        vec![
            mail("cover@x", None, "[PATCH 0/2] series", "Leonardo"),
            mail("p1@x", Some("cover@x"), "[PATCH 1/2] first", "Leonardo"),
            mail("p2@x", Some("cover@x"), "[PATCH 2/2] second", "Leonardo"),
            mail("r1@x", Some("p1@x"), "Re: [PATCH 1/2] first", "Frederic"),
            mail("r2@x", Some("p2@x"), "Re: [PATCH 2/2] second", "Sebastian"),
            // Two months on, answering both reviews above.
            mail("late1@x", Some("r1@x"), "Re: [PATCH 1/2] first", "Leonardo"),
            mail(
                "late2@x",
                Some("r2@x"),
                "Re: [PATCH 2/2] second",
                "Leonardo",
            ),
        ]
    }

    #[test]
    fn thread_order_follows_the_reply_graph_not_the_clock() {
        let ordered = in_thread_order(late_replies_thread());
        let ids: Vec<&str> = ordered
            .iter()
            .filter_map(|mail| mail.message_id.as_deref())
            .map(normalize_message_id)
            .collect();
        // Each patch is followed by its own discussion, the way lore's web
        // view nests it — not both patches first and the late answers last.
        assert_eq!(
            ids,
            [
                "cover@x", "p1@x", "r1@x", "late1@x", "p2@x", "r2@x", "late2@x"
            ]
        );
    }

    #[test]
    fn thread_order_is_stable_under_a_second_pass() {
        // The overview rebuilds the tree from the already-reordered thread,
        // so the walk must reproduce itself: same rows, now in index order.
        let ordered = in_thread_order(late_replies_thread());
        let rows: Vec<(usize, usize)> = thread_tree(&ordered)
            .iter()
            .map(|row| (row.index, row.depth))
            .collect();
        assert_eq!(
            rows,
            [(0, 0), (1, 1), (2, 2), (3, 3), (4, 1), (5, 2), (6, 3)]
        );
    }

    #[test]
    fn tree_covers_the_fixture_thread() {
        let thread = parse_thread(RAW_THREAD);
        let rows = thread_tree(&thread);
        assert_eq!(rows.len(), thread.len());
        // Every message appears exactly once.
        let mut seen: Vec<usize> = rows.iter().map(|row| row.index).collect();
        seen.sort();
        assert_eq!(seen, (0..thread.len()).collect::<Vec<_>>());
        // The OP heads the tree; every reply sits below some parent.
        assert_eq!((rows[0].index, rows[0].depth), (0, 0));
        assert!(rows[1..].iter().all(|row| row.depth > 0));
    }

    #[test]
    fn overview_rows_show_the_subject_and_author() {
        let op = mail("op@x", None, "[PATCH 0/2] series", "Nika Krasnova <nika@x>");
        let reply = mail(
            "a@x",
            Some("op@x"),
            "Re: [PATCH 0/2] series",
            "Miguel Ojeda <m@x>",
        );

        // Every row is titled by its own subject with author and date below.
        assert_eq!(
            overview_row_texts(&op),
            (
                "[PATCH 0/2] series".to_string(),
                "Nika Krasnova · 2026-06-29 03:51".to_string()
            )
        );
        // A reply keeps its own Re:-prefixed subject rather than dropping it.
        assert_eq!(
            overview_row_texts(&reply),
            (
                "Re: [PATCH 0/2] series".to_string(),
                "Miguel Ojeda · 2026-06-29 03:51".to_string()
            )
        );

        // A genuinely empty subject gets a readable placeholder.
        let blank = mail("b@x", None, "", "A");
        assert_eq!(overview_row_texts(&blank).0, "(no subject)");
    }

    #[test]
    fn overview_dates_fall_back_verbatim() {
        let mut broken = mail("x@x", None, "s", "A");
        broken.date = "not a date".to_string();
        assert_eq!(overview_date(&broken.date), "not a date");
    }

    #[test]
    fn opened_index_finds_the_message_the_thread_was_reached_through() {
        let thread = [
            mail("op@x", None, "[PATCH 0/2] series", "Nika"),
            mail("a@x", Some("op@x"), "Re: [PATCH 0/2] series", "Miguel"),
            mail("b@x", Some("op@x"), "[PATCH 1/2] first", "Nika"),
        ];
        // Bare and bracketed ids both resolve to the same message.
        assert_eq!(opened_message_index(&thread, "a@x"), 1);
        assert_eq!(opened_message_index(&thread, "<a@x>"), 1);
        assert_eq!(opened_message_index(&thread, "<b@x> stray"), 2);
        // An unknown or empty id falls back to the OP.
        assert_eq!(opened_message_index(&thread, "gone@x"), 0);
        assert_eq!(opened_message_index(&thread, ""), 0);
    }

    #[test]
    fn normalizes_message_id_references() {
        assert_eq!(normalize_message_id("<a@b>"), "a@b");
        assert_eq!(normalize_message_id(" <a@b> <c@d>"), "a@b");
        assert_eq!(normalize_message_id("bare@id "), "bare@id");
        assert_eq!(author_name("\"Nika K\" <n@x>"), "Nika K");
        assert_eq!(author_name("n@x"), "n@x");
    }

    #[test]
    fn strips_nested_comments() {
        assert_eq!(
            strip_rfc5322_comments("a@b.com (foo (bar) baz), c@d.com"),
            "a@b.com , c@d.com"
        );
        assert_eq!(
            strip_rfc5322_comments(r#""quoted (not comment)" <a@b.com>"#),
            r#""quoted (not comment)" <a@b.com>"#
        );
    }

    #[test]
    fn body_matches_are_case_insensitive_and_non_overlapping() {
        let ranges = body_matches("Fix the Frobnicator, frob it.", &fold_query("frob"));
        // "Frob" at char 8 and "frob" at char 21, ignoring case.
        assert_eq!(ranges, [(8, 12), (21, 25)]);

        // Overlapping needle only matches once per stride.
        assert_eq!(body_matches("aaaa", &fold_query("aa")), [(0, 2), (2, 4)]);

        // An empty query and a no-hit query both yield nothing.
        assert!(body_matches("anything", &fold_query("")).is_empty());
        assert!(body_matches("anything", &fold_query("zzz")).is_empty());
    }

    #[test]
    fn body_matches_offsets_count_characters_not_bytes() {
        // A multi-byte char before the hit must not shift its character offset:
        // "é" is two bytes but one character, so "beta" starts at char 5.
        let ranges = body_matches("café beta", &fold_query("beta"));
        assert_eq!(ranges, [(5, 9)]);

        // Case folding on non-ASCII stays one-to-one, keeping offsets aligned.
        let ranges = body_matches("Straße ödipus", &fold_query("Ödipus"));
        assert_eq!(ranges, [(7, 13)]);
    }
}
