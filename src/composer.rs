use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::highlight;
use crate::settings;

/// Placeholder identity until account support exists.
const IDENTITY: &str = "nika <nika@nikableh.moe>";

const WRAP_WIDTH: usize = 72;

const TRAILERS: [&str; 4] = ["Reviewed-by", "Acked-by", "Tested-by", "Signed-off-by"];

/// Prefill data for a reply, derived from the mail being viewed.
#[derive(Clone)]
pub struct ReplyContext {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub in_reply_to: String,
}

/// The shared editing state: the compact composer and the fullscreen dialog
/// both render these same buffers, so edits stay in sync with no copying.
#[derive(Clone)]
struct ComposerState {
    subject: gtk::EntryBuffer,
    to: gtk::EntryBuffer,
    cc: gtk::EntryBuffer,
    in_reply_to: gtk::EntryBuffer,
    body: gtk::TextBuffer,
    // The context the composer resets against; a cell because a per-mail
    // Reply button can retarget the whole composer at another message.
    initial: Rc<RefCell<ReplyContext>>,
}

impl ComposerState {
    fn new(reply: ReplyContext) -> Self {
        let body = gtk::TextBuffer::new(None);
        // Prefill the signature before enabling undo so it is part of the
        // baseline document rather than an undoable edit.
        prefill_signature(&body);
        body.set_enable_undo(true);
        Self {
            subject: gtk::EntryBuffer::new(Some(&reply.subject)),
            to: gtk::EntryBuffer::new(Some(&reply.to)),
            cc: gtk::EntryBuffer::new(Some(&reply.cc)),
            in_reply_to: gtk::EntryBuffer::new(Some(&reply.in_reply_to)),
            body,
            initial: Rc::new(RefCell::new(reply)),
        }
    }

    fn body_text(&self) -> String {
        let (start, end) = self.body.bounds();
        self.body.text(&start, &end, false).into()
    }

    /// Replace the body in one undoable step. `TextBuffer::set_text` wraps its
    /// delete+insert in an *irreversible* action, which drops the undo stack
    /// entirely, so edits the user should be able to take back (rewrap,
    /// trailers) run the two halves inside a user action instead.
    fn replace_body_text(&self, text: &str) {
        let caret = self
            .body
            .iter_at_mark(&self.body.get_insert())
            .offset()
            .clamp(0, text.chars().count() as i32);

        self.body.begin_user_action();
        let (mut start, mut end) = self.body.bounds();
        self.body.delete(&mut start, &mut end);
        self.body.insert(&mut start, text);
        self.body.end_user_action();

        self.body.place_cursor(&self.body.iter_at_offset(caret));
    }

    fn raw_message(&self) -> String {
        assemble_raw(
            &self.to.text(),
            &self.cc.text(),
            &self.subject.text(),
            &self.in_reply_to.text(),
            &self.body_text(),
        )
    }

    fn reset(&self) {
        let initial = self.initial.borrow();
        self.subject.set_text(&initial.subject);
        self.to.set_text(&initial.to);
        self.cc.set_text(&initial.cc);
        self.in_reply_to.set_text(&initial.in_reply_to);
        // Re-read the signature so a change made in Preferences takes effect on
        // the next fresh reply, without waiting for a restart.
        prefill_signature(&self.body);
    }

    /// Swap in a new reply target: the header fields follow the new context
    /// (and the subject revert icon and Discard now reset against it), but
    /// any body text already typed is deliberately kept.
    fn retarget(&self, reply: ReplyContext) {
        // The context goes in first so the entry change handlers (the
        // subject revert icon) compare against the new target, and the
        // subject is cleared before being set so a change always fires
        // even when the old draft already carried the new subject.
        self.initial.replace(reply);
        let initial = self.initial.borrow();
        self.subject.set_text("");
        self.subject.set_text(&initial.subject);
        self.to.set_text(&initial.to);
        self.cc.set_text(&initial.cc);
        self.in_reply_to.set_text(&initial.in_reply_to);
    }
}

/// Hide the stock "Insert Emoji" and "Change Direction" items from the
/// context menu of a composer input. For entries the actions live on the
/// internal GtkText delegate; the emoji item must be suppressed via
/// InputHints::NO_EMOJI because GTK re-enables the action whenever the
/// hints or editability change.
fn strip_extra_context_items(widget: &impl IsA<gtk::Widget>) {
    let widget = widget.upcast_ref::<gtk::Widget>();
    if let Some(entry) = widget.downcast_ref::<gtk::Entry>() {
        entry.set_input_hints(entry.input_hints() | gtk::InputHints::NO_EMOJI);
        if let Some(text) = entry.first_child().and_downcast::<gtk::Text>() {
            text.action_set_enabled("misc.toggle-direction", false);
        }
    } else if let Some(view) = widget.downcast_ref::<gtk::TextView>() {
        // GtkTextView has no "Change Direction" item, only the emoji one.
        view.set_input_hints(view.input_hints() | gtk::InputHints::NO_EMOJI);
    }
}

/// Show a revert icon in a subject entry whenever its text differs from the
/// prefilled reply subject; clicking the icon restores it.
fn setup_subject_revert(entry: &gtk::Entry, initial: &Rc<RefCell<ReplyContext>>) {
    let initial = initial.clone();

    entry.set_secondary_icon_activatable(true);
    entry.set_secondary_icon_tooltip_text(Some("Revert Subject"));

    let apply = glib::clone!(
        #[strong]
        initial,
        move |entry: &gtk::Entry| {
            let modified = entry.text() != initial.borrow().subject;
            entry.set_secondary_icon_name(modified.then_some("edit-undo-symbolic"));
        }
    );
    apply(entry);
    entry.connect_changed(move |entry| apply(entry));

    entry.connect_icon_release(move |entry, position| {
        if position == gtk::EntryIconPosition::Secondary {
            let subject = initial.borrow().subject.clone();
            entry.set_text(&subject);
        }
    });
}

/// Prefix `subject` with "Re: " unless it already carries one.
pub fn reply_subject(subject: &str) -> String {
    if subject.to_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// Append `trailer` on its own line at the end of `body`: exactly one
/// newline separates it from a nonempty body, and it always ends the text
/// with a trailing newline.
fn append_trailer(body: &str, trailer: &str) -> String {
    let mut out = body.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(trailer);
    out.push('\n');
    out
}

/// Fill `body` with the current reply signature, read live from settings so a
/// Preferences edit reaches the next fresh reply without a restart. The cursor
/// is left on the empty line above it; when no signature is configured the body
/// is simply cleared.
fn prefill_signature(body: &gtk::TextBuffer) {
    let signature = settings::reply_signature();
    if signature.is_empty() {
        body.set_text("");
    } else {
        body.set_text(&format!("\n\n{signature}\n"));
        body.place_cursor(&body.start_iter());
    }
}

/// Append `signature` (which carries its own `-- ` separator) at the end of
/// `body`, set off from any typed text by one blank line, the way a signature
/// conventionally sits below a mail, and ending with a trailing newline.
fn append_signature(body: &str, signature: &str) -> String {
    let mut out = body.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(signature);
    out.push('\n');
    out
}

/// Greedy-wrap `text` at `width` columns on word boundaries. Blank lines are
/// kept as paragraph breaks, quoted lines (starting with ">") pass through
/// untouched, and words longer than `width` (long URLs) stay on their own
/// line unbroken. Width is counted in chars, not display cells, so wide CJK
/// glyphs count as one column.
fn rewrap(text: &str, width: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut para: Vec<&str> = Vec::new();

    let flush = |para: &mut Vec<&str>, out: &mut Vec<String>| {
        let mut line = String::new();
        let mut line_len = 0usize;
        for word in para.iter().flat_map(|l| l.split_whitespace()) {
            let word_len = word.chars().count();
            if line.is_empty() {
                line.push_str(word);
                line_len = word_len;
            } else if line_len + 1 + word_len <= width {
                line.push(' ');
                line.push_str(word);
                line_len += 1 + word_len;
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
                line_len = word_len;
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
        para.clear();
    };

    for raw_line in text.lines() {
        if raw_line.trim().is_empty() {
            flush(&mut para, &mut out);
            out.push(String::new());
        } else if raw_line.starts_with('>') {
            flush(&mut para, &mut out);
            out.push(raw_line.to_string());
        } else {
            para.push(raw_line);
        }
    }
    flush(&mut para, &mut out);

    let mut result = out.join("\n");
    if text.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

/// Assemble the raw RFC 5322-style reply shown by the Raw Preview toggle.
fn assemble_raw(to: &str, cc: &str, subject: &str, in_reply_to: &str, body: &str) -> String {
    let mut raw = format!("From: {IDENTITY}\nTo: {to}\n");
    if !cc.is_empty() {
        raw.push_str("Cc: ");
        raw.push_str(cc);
        raw.push('\n');
    }
    raw.push_str("Subject: ");
    raw.push_str(subject);
    raw.push('\n');
    if !in_reply_to.is_empty() {
        raw.push_str("In-Reply-To: ");
        raw.push_str(in_reply_to);
        raw.push('\n');
    }
    raw.push('\n');
    raw.push_str(body);
    raw
}

/// A handle to a built composer: the bottom-bar widget to insert into the
/// page, plus the hooks a per-mail Reply button needs to retarget the
/// draft at another message in the thread.
#[derive(Clone)]
pub struct Composer {
    widget: gtk::Box,
    state: ComposerState,
    expand_toggle: gtk::ToggleButton,
    body_view: gtk::TextView,
    preview_toggle: gtk::ToggleButton,
}

impl Composer {
    pub fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Point the composer at `reply`: headers and the reset baseline follow
    /// the new context, typed body text is kept, and the expanded editor is
    /// shown and focused.
    pub fn start_reply(&self, reply: ReplyContext) {
        self.state.retarget(reply);
        self.expand_toggle.set_active(true);
        // Leave an active Raw Preview: replying means editing, and the
        // focus grab below only lands once the editor page is mapped.
        self.preview_toggle.set_active(false);
        self.body_view.grab_focus();
    }

    /// Insert `quoted` into the body at the last known cursor position and
    /// reveal the editor with the cursor on the line after the quote.
    pub fn insert_quote(&self, quoted: &str) {
        let buffer = &self.state.body;
        let mut iter = buffer.iter_at_mark(&buffer.get_insert());
        // Keep the quote on lines of its own: break out of a partially
        // typed line first, and end with a newline so the cursor lands on
        // the line after the quote.
        let mut text = String::new();
        if !iter.starts_line() {
            text.push('\n');
        }
        text.push_str(quoted);
        text.push('\n');
        buffer.insert(&mut iter, &text);
        buffer.place_cursor(&iter);

        self.expand_toggle.set_active(true);
        self.preview_toggle.set_active(false);
        self.body_view.grab_focus();
        // Bring the cursor into view once the editor has a real allocation:
        // the expander may only be expanding now, and scrolling a view that
        // isn't laid out yet is a no-op.
        glib::idle_add_local_once(glib::clone!(
            #[weak(rename_to = view)]
            self.body_view,
            move || {
                let buffer = view.buffer();
                view.scroll_to_mark(&buffer.get_insert(), 0.0, false, 0.0, 0.0);
            }
        ));
    }
}

/// Build the reply composer: one click-anywhere bar that is the bottom bar
/// when collapsed and the editor's header when open, toggling the editor
/// either way with a chevron that flips to match. The bar's label and the
/// open editor are clamped to the mail page's reading width so they line up
/// with the message column.
pub fn build_composer(reply: ReplyContext) -> Composer {
    let state = ComposerState::new(reply);

    let subject_entry = gtk::Entry::builder()
        .buffer(&state.subject)
        .placeholder_text("Subject")
        .hexpand(true)
        .build();
    strip_extra_context_items(&subject_entry);
    setup_subject_revert(&subject_entry, &state.initial);

    // One title column shared by the Subject label and the headers grid,
    // so all four entries start at the same x when Details is open.
    let titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);

    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&build_headers_grid(&state, &titles))
        .build();

    let details_toggle = gtk::ToggleButton::builder()
        .icon_name("view-more-horizontal-symbolic")
        .tooltip_text("Details")
        .css_classes(["flat"])
        .build();
    details_toggle
        .bind_property("active", &revealer, "reveal-child")
        .sync_create()
        .build();

    // Editor <-> raw preview stack. The preview gets its own buffer because
    // it shows the assembled message, not the editable body.
    let preview_buffer = gtk::TextBuffer::new(None);
    let preview_view = gtk::TextView::builder()
        .buffer(&preview_buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    let preview_scrolled = gtk::ScrolledWindow::builder()
        .child(&preview_view)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(300)
        .max_content_height(560)
        .propagate_natural_height(true)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let (body_editor, body_view) = build_body_editor(&state.body, true);
    stack.add_named(&body_editor, Some("edit"));
    stack.add_named(&preview_scrolled, Some("preview"));

    let preview_toggle = gtk::ToggleButton::builder()
        .icon_name("text-x-generic-symbolic")
        .tooltip_text("Raw Preview")
        .css_classes(["flat"])
        .build();
    preview_toggle.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[weak]
        preview_buffer,
        #[weak]
        stack,
        move |toggle| {
            if toggle.is_active() {
                preview_buffer.set_text(&state.raw_message());
                stack.set_visible_child_name("preview");
            } else {
                stack.set_visible_child_name("edit");
            }
        }
    ));

    // Keep an active preview live: the subject entry and toolbar stay usable
    // while the preview is shown, so edits made then must show up in it.
    let refresh_preview: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        state,
        #[weak]
        preview_buffer,
        #[weak]
        preview_toggle,
        move || {
            if preview_toggle.is_active() {
                preview_buffer.set_text(&state.raw_message());
            }
        }
    ));
    state.body.connect_changed(glib::clone!(
        #[strong]
        refresh_preview,
        move |_| refresh_preview()
    ));
    // The fullscreen dialog shares this buffer, so highlighting attached
    // here covers it too.
    highlight::attach(&state.body);
    highlight::refresh(&state.body);
    state.body.connect_changed(highlight::refresh);

    for buffer in [&state.subject, &state.to, &state.cc, &state.in_reply_to] {
        buffer.connect_text_notify(glib::clone!(
            #[strong]
            refresh_preview,
            move |_| refresh_preview()
        ));
    }

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Expansion state lives on a headless toggle so start_reply/insert_quote/
    // Discard can flip it directly; the header bar below binds to it.
    let expand_toggle = gtk::ToggleButton::new();

    // The subject row is a one-row grid sharing the headers grid's column
    // spacing, with its label in the same SizeGroup: the label column and
    // the entry's left edge then match the To/Cc/In-Reply-To rows exactly.
    let subject_label = build_field_label("Subject");
    titles.add_widget(&subject_label);
    let subject_grid = gtk::Grid::builder().column_spacing(12).build();
    subject_grid.attach(&subject_label, 0, 0, 1, 1);
    subject_grid.attach(&subject_entry, 1, 0, 1, 1);

    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .build();
    toolbar.append(&details_toggle);
    toolbar.append(&build_insert_button(&state, &root));
    toolbar.append(&build_rewrap_button(&state));
    toolbar.append(&preview_toggle);
    toolbar.append(&build_fullscreen_toggle(&state));
    toolbar.append(&build_discard_button(
        &state,
        &details_toggle,
        &preview_toggle,
        &expand_toggle,
    ));
    subject_grid.attach(&toolbar, 2, 0, 1, 1);

    root.append(&subject_grid);
    root.append(&revealer);
    root.append(&stack);

    // The open editor keeps the mail column's reading width so its fields
    // line up with the message bodies above it.
    let editor_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&root)
        .build();
    let editor_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .child(&editor_clamp)
        .build();

    // One full-width flat button is the whole composer's face: the bottom
    // bar when collapsed, the editor's header when open. A click anywhere on
    // it toggles the editor, and the chevron flips to show which way it
    // goes. Its icon and label sit in the same reading-width clamp as the
    // thread, so they line up with the message column rather than hugging
    // the window edge.
    let chevron = gtk::Image::from_icon_name("pan-up-symbolic");
    let header_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(9)
        .margin_bottom(9)
        .margin_start(12)
        .margin_end(12)
        .build();
    header_content.append(&gtk::Image::from_icon_name("mail-reply-sender-symbolic"));
    header_content.append(
        &gtk::Label::builder()
            .label("Reply")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .xalign(0.0)
            .build(),
    );
    header_content.append(&chevron);
    let header_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .hexpand(true)
        .child(&header_content)
        .build();
    let header_bar = gtk::Button::builder()
        .css_classes(["flat"])
        .tooltip_text("Expand")
        .child(&header_clamp)
        .build();
    header_bar.connect_clicked(glib::clone!(
        #[weak]
        expand_toggle,
        move |_| expand_toggle.set_active(!expand_toggle.is_active())
    ));

    // The one switch reveals the editor, flips the chevron and retitles the
    // bar; opening also focuses the body, mirroring what start_reply does
    // for per-mail Reply buttons.
    expand_toggle
        .bind_property("active", &editor_revealer, "reveal-child")
        .sync_create()
        .build();
    expand_toggle.connect_toggled(glib::clone!(
        #[weak]
        body_view,
        #[weak]
        chevron,
        #[weak]
        header_bar,
        move |toggle| {
            let open = toggle.is_active();
            chevron.set_icon_name(Some(if open {
                "pan-down-symbolic"
            } else {
                "pan-up-symbolic"
            }));
            header_bar.set_tooltip_text(Some(if open { "Collapse" } else { "Expand" }));
            if open {
                body_view.grab_focus();
            }
        }
    ));

    // The composer floats over the bottom of the mail pane (see thread_page),
    // so it needs an opaque background and a top divider of its own: bottom-
    // anchored, it is the collapsed bar while shut and grows up over the
    // content when open.
    let widget = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::End)
        .css_classes(["background"])
        .build();
    widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    widget.append(&header_bar);
    widget.append(&editor_revealer);

    Composer {
        widget,
        state,
        expand_toggle,
        body_view,
        preview_toggle,
    }
}

/// A caption-heading field-name label, shared by the headers grid and the
/// subject rows so every field title looks the same.
fn build_field_label(name: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(name)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .css_classes(["caption-heading"])
        .build()
}

/// To / Cc / In-Reply-To rows, shared by the revealer and the fullscreen
/// dialog through the state's EntryBuffers. The field labels join `titles`
/// so the entry column lines up with the caller's subject row.
fn build_headers_grid(state: &ComposerState, titles: &gtk::SizeGroup) -> gtk::Grid {
    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(6)
        .build();

    let fields = [
        ("To", &state.to),
        ("Cc", &state.cc),
        ("In-Reply-To", &state.in_reply_to),
    ];
    for (row, (name, buffer)) in fields.into_iter().enumerate() {
        let label = build_field_label(name);
        titles.add_widget(&label);
        let entry = gtk::Entry::builder().buffer(buffer).hexpand(true).build();
        strip_extra_context_items(&entry);
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&entry, 1, row as i32, 1, 1);
    }

    grid
}

/// A monospace body editor with a dim 72-column ruler overlaid at the wrap
/// margin. `compact` limits the height so the sticky bar stays small; the
/// fullscreen dialog passes false and lets the editor fill the dialog.
fn build_body_editor(buffer: &gtk::TextBuffer, compact: bool) -> (gtk::Overlay, gtk::TextView) {
    let view = gtk::TextView::builder()
        .buffer(buffer)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    strip_extra_context_items(&view);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .build();
    if compact {
        scrolled.set_min_content_height(300);
        scrolled.set_max_content_height(560);
        scrolled.set_propagate_natural_height(true);
    } else {
        scrolled.set_vexpand(true);
    }

    let ruler = gtk::Separator::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Fill)
        .can_target(false)
        .opacity(0.4)
        .build();

    let overlay = gtk::Overlay::builder().child(&scrolled).build();
    overlay.add_overlay(&ruler);
    overlay.set_measure_overlay(&ruler, false);
    overlay.set_clip_overlay(&ruler, true);

    // Position the ruler at the 72nd column. Measuring a full 72-char string
    // avoids accumulating per-char rounding error, and the position must
    // track the horizontal scroll offset: the overlay is fixed in viewport
    // coordinates while the unwrapped text scrolls underneath it.
    let position_ruler: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        ruler,
        #[weak]
        view,
        #[weak]
        scrolled,
        move || {
            let layout = view.create_pango_layout(Some(&"0".repeat(WRAP_WIDTH)));
            let offset =
                view.left_margin() + layout.pixel_size().0 - scrolled.hadjustment().value() as i32;
            ruler.set_visible(offset >= 0);
            ruler.set_margin_start(offset.max(0));
        }
    ));
    // On map the pango context carries the real font; the scroll handler
    // keeps the ruler on column 72 as the text pans under the overlay.
    view.connect_map(glib::clone!(
        #[strong]
        position_ruler,
        move |_| position_ruler()
    ));
    scrolled.hadjustment().connect_value_changed(glib::clone!(
        #[strong]
        position_ruler,
        move |_| position_ruler()
    ));

    (overlay, view)
}

fn build_insert_button(state: &ComposerState, action_scope: &gtk::Box) -> gtk::MenuButton {
    let menu = gio::Menu::new();

    let trailers = gio::Menu::new();
    for kind in TRAILERS {
        trailers.append(Some(kind), Some(&format!("composer.trailer('{kind}')")));
    }
    menu.append_section(None, &trailers);

    let group = gio::SimpleActionGroup::new();

    let trailer = gio::SimpleAction::new("trailer", Some(glib::VariantTy::STRING));
    trailer.connect_activate(glib::clone!(
        #[strong]
        state,
        move |_, param| {
            let Some(kind) = param.and_then(|p| p.str()) else {
                return;
            };
            let trailer = format!("{kind}: {IDENTITY}");
            state.replace_body_text(&append_trailer(&state.body_text(), &trailer));
        }
    ));
    group.add_action(&trailer);

    // A signature is unlike a trailer, so it gets its own section in the same
    // menu rather than a button of its own — shown only when one is configured.
    // The action re-reads it so a Preferences edit is reflected right away.
    if !settings::reply_signature().is_empty() {
        let section = gio::Menu::new();
        section.append(Some("Signature"), Some("composer.signature"));
        menu.append_section(None, &section);

        let action = gio::SimpleAction::new("signature", None);
        action.connect_activate(glib::clone!(
            #[strong]
            state,
            move |_, _| {
                let signature = settings::reply_signature();
                if !signature.is_empty() {
                    state.replace_body_text(&append_signature(&state.body_text(), &signature));
                }
            }
        ));
        group.add_action(&action);
    }

    action_scope.insert_action_group("composer", Some(&group));

    gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Insert")
        .menu_model(&menu)
        .css_classes(["flat"])
        .build()
}

fn build_rewrap_button(state: &ComposerState) -> gtk::Button {
    let button = gtk::Button::builder()
        // Bundled icon: icon-development-kit's arrow-hook-left-horizontal2
        // flipped vertically (see data/icons/).
        .icon_name("koshi-rewrap-symbolic")
        .tooltip_text("Rewrap Lines")
        .css_classes(["flat"])
        .build();
    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| {
            state.replace_body_text(&rewrap(&state.body_text(), WRAP_WIDTH));
        }
    ));
    button
}

fn build_fullscreen_toggle(state: &ComposerState) -> gtk::ToggleButton {
    let toggle = gtk::ToggleButton::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Fullscreen")
        .css_classes(["flat"])
        .build();

    let open_dialog: Rc<RefCell<Option<adw::Dialog>>> = Rc::new(RefCell::new(None));
    toggle.connect_toggled(glib::clone!(
        #[strong]
        state,
        #[strong]
        open_dialog,
        move |toggle| {
            if toggle.is_active() {
                if open_dialog.borrow().is_some() {
                    return;
                }
                let dialog = build_fullscreen_dialog(&state);
                dialog.connect_closed(glib::clone!(
                    #[weak]
                    toggle,
                    #[strong]
                    open_dialog,
                    move |_| {
                        open_dialog.replace(None);
                        toggle.set_active(false);
                    }
                ));
                // Store the dialog before presenting: connect_closed clears
                // the slot, so storing after present() would re-fill it with
                // an already-closed dialog if close ever fired synchronously.
                open_dialog.replace(Some(dialog.clone()));
                dialog.present(Some(toggle));
            } else if let Some(dialog) = open_dialog.take() {
                dialog.close();
            }
        }
    ));

    toggle
}

/// The focused-editing view: same buffers, roomier layout, headers always
/// shown. Closing it lands back on the sticky bar with every edit intact.
fn build_fullscreen_dialog(state: &ComposerState) -> adw::Dialog {
    let subject_entry = gtk::Entry::builder()
        .buffer(&state.subject)
        .placeholder_text("Subject")
        .hexpand(true)
        .build();
    strip_extra_context_items(&subject_entry);
    setup_subject_revert(&subject_entry, &state.initial);

    // Same title-column trick as the compact composer: the Subject label
    // shares a SizeGroup with the grid labels so the entries align. The
    // row spacing already matches the grid's column spacing (12).
    let titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    let subject_label = build_field_label("Subject");
    titles.add_widget(&subject_label);

    let subject_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    subject_row.append(&subject_label);
    subject_row.append(&subject_entry);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&subject_row);
    content.append(&build_headers_grid(state, &titles));
    content.append(&build_body_editor(&state.body, false).0);

    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&content)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&clamp));

    adw::Dialog::builder()
        .title("Reply")
        .content_width(900)
        .content_height(700)
        .child(&toolbar_view)
        .build()
}

fn build_discard_button(
    state: &ComposerState,
    details_toggle: &gtk::ToggleButton,
    preview_toggle: &gtk::ToggleButton,
    expand_toggle: &gtk::ToggleButton,
) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Discard")
        .css_classes(["flat"])
        .build();

    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        details_toggle,
        #[weak]
        preview_toggle,
        #[weak]
        expand_toggle,
        move |button| {
            let dialog = adw::AlertDialog::new(
                Some("Discard Draft?"),
                Some("The draft will be reset to the original reply"),
            );
            dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard")]);
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            dialog.connect_response(
                Some("discard"),
                glib::clone!(
                    #[strong]
                    state,
                    #[weak]
                    details_toggle,
                    #[weak]
                    preview_toggle,
                    #[weak]
                    expand_toggle,
                    move |_, _| {
                        state.reset();
                        details_toggle.set_active(false);
                        preview_toggle.set_active(false);
                        expand_toggle.set_active(false);
                    }
                ),
            );
            dialog.present(Some(button));
        }
    ));

    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrap_wraps_long_lines_at_word_boundaries() {
        let input = "one two three ".repeat(10); // 140 chars on one line
        let wrapped = rewrap(input.trim_end(), 72);
        assert!(wrapped.lines().count() > 1);
        for line in wrapped.lines() {
            assert!(line.chars().count() <= 72, "line too long: {line:?}");
            assert!(!line.starts_with(' ') && !line.ends_with(' '));
        }
        // No words lost or reordered.
        let words: Vec<&str> = wrapped.split_whitespace().collect();
        assert_eq!(words.len(), 30);
        assert_eq!(
            wrapped.split_whitespace().collect::<Vec<_>>(),
            input.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn rewrap_leaves_quoted_lines_untouched() {
        let quoted = format!("> {}", "quoted words repeated ".repeat(8).trim_end());
        let input = format!("{quoted}\n\nA reply line");
        let wrapped = rewrap(&input, 72);
        assert!(wrapped.lines().next().unwrap() == quoted, "quote rewrapped");
        assert!(wrapped.ends_with("A reply line"));
    }

    #[test]
    fn rewrap_preserves_paragraph_breaks() {
        let long = "word ".repeat(30);
        let input = format!("{}\n\n{}", long.trim_end(), "short second paragraph");
        let wrapped = rewrap(&input, 72);
        assert!(wrapped.contains("\n\n"), "blank line lost: {wrapped:?}");
        assert!(wrapped.ends_with("short second paragraph"));
        // Consecutive lines of one paragraph are joined before wrapping.
        assert_eq!(rewrap("joined\nacross\nlines", 72), "joined across lines");
    }

    #[test]
    fn rewrap_keeps_unbreakable_words_whole() {
        let url = format!("https://example.com/{}", "x".repeat(80));
        let input = format!("see {url} for details");
        let wrapped = rewrap(&input, 72);
        assert!(
            wrapped.lines().any(|line| line == url),
            "URL broken: {wrapped:?}"
        );
    }

    #[test]
    fn rewrap_preserves_trailing_newline() {
        assert_eq!(rewrap("line\n", 72), "line\n");
        assert_eq!(rewrap("line", 72), "line");
    }

    #[test]
    fn rewrap_is_idempotent() {
        let input = format!(
            "{}\n> quoted line kept verbatim\n\nsecond paragraph {}\n",
            "word ".repeat(40).trim_end(),
            "tail ".repeat(30).trim_end()
        );
        let once = rewrap(&input, 72);
        assert_eq!(rewrap(&once, 72), once);
    }

    #[test]
    fn append_trailer_manages_newlines() {
        let trailer = "Reviewed-by: nika <nika@nikableh.moe>";
        assert_eq!(append_trailer("", trailer), format!("{trailer}\n"));
        assert_eq!(
            append_trailer("body", trailer),
            format!("body\n{trailer}\n")
        );
        assert_eq!(
            append_trailer("body\n", trailer),
            format!("body\n{trailer}\n")
        );
    }

    #[test]
    fn append_signature_separates_with_blank_line() {
        // The signature already carries its own `-- ` separator; append only
        // handles the spacing. Onto an empty body it stands alone; onto typed
        // text it is set off by exactly one blank line regardless of the body's
        // trailing newlines.
        let sig = "-- \nNika Krasnova";
        assert_eq!(append_signature("", sig), "-- \nNika Krasnova\n");
        assert_eq!(
            append_signature("body", sig),
            "body\n\n-- \nNika Krasnova\n"
        );
        assert_eq!(
            append_signature("body\n\n\n", sig),
            "body\n\n-- \nNika Krasnova\n"
        );
    }

    #[test]
    fn reply_subject_prefixes_once() {
        assert_eq!(reply_subject("[PATCH] fix"), "Re: [PATCH] fix");
        assert_eq!(reply_subject("Re: [PATCH] fix"), "Re: [PATCH] fix");
        assert_eq!(reply_subject("RE: shouting"), "RE: shouting");
    }

    #[test]
    fn assemble_raw_skips_empty_optional_headers() {
        let raw = assemble_raw("a@b", "", "Subj", "", "body");
        assert_eq!(
            raw,
            format!("From: {IDENTITY}\nTo: a@b\nSubject: Subj\n\nbody")
        );
        let full = assemble_raw("a@b", "c@d", "Subj", "<id@x>", "body");
        assert!(full.contains("\nCc: c@d\n"));
        assert!(full.contains("\nIn-Reply-To: <id@x>\n\nbody"));
    }
}
