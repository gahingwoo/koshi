use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::highlight;
use crate::message;
use crate::profile;
use crate::send;
use crate::settings;

/// The `User-Agent` header value announcing Koshi as the mail client, e.g.
/// `koshi/0.1.0`. Prefilled into a reply only when [`settings::send_user_agent`]
/// is on; the user can still delete the header from the message before sending.
const USER_AGENT: &str = concat!("koshi/", env!("CARGO_PKG_VERSION"));

const WRAP_WIDTH: usize = 72;

const TRAILERS: [&str; 4] = ["Reviewed-by", "Acked-by", "Tested-by", "Signed-off-by"];

/// Prefill data for a reply, derived from the mail being viewed.
#[derive(Clone)]
pub struct ReplyContext {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub in_reply_to: String,
    /// The parent message's `References` header, carried (not edited) so the
    /// reply can extend the chain.
    pub references: String,
}

impl ReplyContext {
    /// An empty context for composing a brand-new message: no recipients,
    /// subject or threading headers. The prefill is then just the sender's
    /// identity and signature.
    pub fn blank() -> Self {
        ReplyContext {
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            in_reply_to: String::new(),
            references: String::new(),
        }
    }
}

/// The editing state. The composer is the raw message: a single buffer holds
/// the prefilled headers, a blank line, and the body, and the user edits all of
/// it in place. The compact composer and the fullscreen dialog share this one
/// buffer, so edits stay in sync with no copying.
#[derive(Clone)]
struct ComposerState {
    document: gtk::TextBuffer,
    // The context the document resets against; a cell because a per-mail Reply
    // button can retarget the whole composer at another message.
    initial: Rc<RefCell<ReplyContext>>,
}

impl ComposerState {
    fn new(reply: ReplyContext) -> Self {
        let document = gtk::TextBuffer::new(None);
        let doc = document_for(&reply);
        // Set the prefill before enabling undo so the baseline document is not
        // itself an undoable edit.
        document.set_text(&doc);
        document.set_enable_undo(true);
        let state = Self {
            document,
            initial: Rc::new(RefCell::new(reply)),
        };
        state.place_cursor_at_body(&doc);
        state
    }

    fn document_text(&self) -> String {
        let (start, end) = self.document.bounds();
        self.document.text(&start, &end, false).into()
    }

    /// Replace the whole document (headers and body) and drop the cursor onto
    /// the first body line. Clears the undo stack — used for reset/retarget,
    /// where taking the previous draft back is not wanted.
    fn set_document(&self, doc: &str) {
        self.document.set_text(doc);
        self.place_cursor_at_body(doc);
    }

    fn place_cursor_at_body(&self, doc: &str) {
        let offset = body_start_offset(doc);
        self.document
            .place_cursor(&self.document.iter_at_offset(offset));
    }

    /// The body region — everything after the header block's blank-line
    /// separator — as the user currently has it.
    fn body_text(&self) -> String {
        let (_, body) = message::parse_headers(&self.document_text());
        body
    }

    /// Replace just the body region in one undoable step, leaving the headers
    /// untouched. `TextBuffer::set_text` would wrap delete+insert in an
    /// *irreversible* action that drops the undo stack, so edits the user should
    /// be able to take back (rewrap, trailers) run inside a user action instead.
    fn replace_body(&self, body: &str) {
        let start_offset = body_start_offset(&self.document_text());
        let caret = self
            .document
            .iter_at_mark(&self.document.get_insert())
            .offset();

        self.document.begin_user_action();
        let mut start = self.document.iter_at_offset(start_offset);
        let mut end = self.document.end_iter();
        self.document.delete(&mut start, &mut end);
        self.document.insert(&mut start, body);
        self.document.end_user_action();

        let caret = caret.clamp(start_offset, self.document.end_iter().offset());
        self.document
            .place_cursor(&self.document.iter_at_offset(caret));
    }

    /// Whether the body is still the untouched prefill (empty or just the
    /// signature) — used to decide whether retargeting at another message can
    /// happen silently or should first confirm discarding a typed draft.
    fn body_is_pristine(&self) -> bool {
        self.body_text().trim() == reply_body_seed().trim()
    }

    fn reset(&self) {
        let doc = document_for(&self.initial.borrow());
        self.set_document(&doc);
    }

    /// Point the composer at a new reply target: the whole document is rebuilt
    /// against the new context (and Discard now resets against it).
    fn retarget(&self, reply: ReplyContext) {
        let doc = document_for(&reply);
        self.initial.replace(reply);
        self.set_document(&doc);
    }
}

/// The raw message to prefill a reply with: the headers from `reply` plus the
/// sender's identity and (unless opted out) a `User-Agent`, then the body seed.
fn document_for(reply: &ReplyContext) -> String {
    message::render(&message::Outgoing {
        from: sender_identity(),
        to: reply.to.clone(),
        cc: reply.cc.clone(),
        subject: reply.subject.clone(),
        in_reply_to: reply.in_reply_to.clone(),
        references: reply.references.clone(),
        user_agent: settings::send_user_agent().then(|| USER_AGENT.to_string()),
        body: reply_body_seed(),
    })
}

/// The initial body of a reply: the configured signature set off by a blank
/// line for the reply to be typed above, read live from settings so a
/// Preferences edit reaches the next fresh reply. Empty when no signature is
/// configured — just a blank line to type on.
fn reply_body_seed() -> String {
    let signature = settings::reply_signature();
    if signature.is_empty() {
        String::new()
    } else {
        format!("\n\n{signature}\n")
    }
}

/// The char offset of the first body character: just past the blank line that
/// separates the header block from the body. The whole length when there is no
/// blank line (an all-headers document).
fn body_start_offset(doc: &str) -> i32 {
    match doc.find("\n\n") {
        Some(byte) => doc[..byte + 2].chars().count() as i32,
        None => doc.chars().count() as i32,
    }
}

/// Ensure the document ends with exactly one trailing newline, as git wants the
/// message file to. The buffer already uses LF line endings.
fn normalize_document(doc: &str) -> String {
    let mut out = doc.trim_end_matches('\n').to_string();
    out.push('\n');
    out
}

/// Hide the stock "Insert Emoji" and "Change Direction" items from the context
/// menu of a composer input. GtkTextView has only the emoji item, suppressed
/// via InputHints::NO_EMOJI because GTK re-enables the action whenever the hints
/// or editability change.
fn strip_extra_context_items(view: &gtk::TextView) {
    view.set_input_hints(view.input_hints() | gtk::InputHints::NO_EMOJI);
}

/// Prefix `subject` with "Re: " unless it already carries one.
pub fn reply_subject(subject: &str) -> String {
    if subject.to_lowercase().starts_with("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// Append `trailer` on its own line at the end of `body`: exactly one newline
/// separates it from a nonempty body, and it always ends the text with a
/// trailing newline.
fn append_trailer(body: &str, trailer: &str) -> String {
    let mut out = body.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(trailer);
    out.push('\n');
    out
}

/// Append `signature` (which carries its own `-- ` separator) at the end of
/// `body`, set off from any typed text by one blank line, and ending with a
/// trailing newline.
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
/// kept as paragraph breaks; structural lines pass through untouched — quoted
/// lines and every part of a unified diff (see [`highlight::preserve_mask`]) —
/// and a trailing copy of `signature` is preserved verbatim so a configured
/// signature at the end of the body is never reflowed. Words longer than
/// `width` (long URLs) stay on their own line unbroken. Width is counted in
/// chars, not display cells, so wide CJK glyphs count as one column.
fn rewrap(text: &str, width: usize, signature: &str) -> String {
    // Peel a signature block off the end so it survives verbatim. It only
    // counts as the signature when it sits at the very end of the body; a
    // matching block with prose after it is just text, and gets rewrapped.
    let (text, sig_tail) = split_signature(text, signature);

    let mask = highlight::preserve_mask(text);
    let lines: Vec<&str> = text.split('\n').collect();
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

    for (i, raw_line) in lines.iter().enumerate() {
        if mask[i] {
            flush(&mut para, &mut out);
            out.push(raw_line.to_string());
        } else if raw_line.trim().is_empty() {
            flush(&mut para, &mut out);
            out.push(String::new());
        } else {
            para.push(raw_line);
        }
    }
    flush(&mut para, &mut out);

    let mut result = out.join("\n");
    result.push_str(sig_tail);
    result
}

/// Split a trailing `signature` block off `text`, returning `(head, tail)`
/// where `tail` is the verbatim signature (including any trailing newlines)
/// and `head` is everything before it. The signature must sit at the end of
/// the body on a line boundary; otherwise the whole text is the head and the
/// tail is empty. An empty configured signature never matches.
fn split_signature<'a>(text: &'a str, signature: &str) -> (&'a str, &'a str) {
    let sig = signature.trim_end_matches('\n');
    if sig.is_empty() {
        return (text, "");
    }
    let end = text.trim_end_matches('\n').len();
    match end.checked_sub(sig.len()) {
        Some(start)
            if &text[start..end] == sig && (start == 0 || text.as_bytes()[start - 1] == b'\n') =>
        {
            (&text[..start], &text[start..])
        }
        _ => (text, ""),
    }
}

/// The `From:` identity for a reply — the sender git will actually send as,
/// read from git config. Empty when git has no address configured (the send
/// path surfaces that; the prefill just shows a blank `From:`).
fn sender_identity() -> String {
    profile::cached().sender_header().unwrap_or_default()
}

/// Where a composer lives, and so how it returns to rest after sending or
/// discarding. The two surfaces share one editor build; only the dismissal
/// differs.
#[derive(Clone)]
enum Surface {
    /// The bottom-sheet composer: reset the draft and close the sheet back
    /// down to its bottom bar.
    Inline(adw::BottomSheet),
    /// A standalone full-page composer: close the tab it lives in.
    Tab,
}

impl Surface {
    /// Return the composer to rest after a completed send or an explicit
    /// discard. `from` is any widget inside the composer, used by the tab
    /// surface to find and close its own tab.
    fn dismiss(&self, state: &ComposerState, from: &impl IsA<gtk::Widget>) {
        match self {
            Surface::Inline(sheet) => {
                state.reset();
                sheet.set_open(false);
            }
            Surface::Tab => close_composer_tab(from),
        }
    }
}

/// Close the tab `widget` sits in: its `NavigationView` is the tab's child, so
/// the enclosing `TabView` can find and close the page. A no-op if the composer
/// is not in a tab (it always is here, but the walk-up stays defensive).
fn close_composer_tab(widget: &impl IsA<gtk::Widget>) {
    let Some(nav) = widget
        .ancestor(adw::NavigationView::static_type())
        .and_downcast::<adw::NavigationView>()
    else {
        return;
    };
    let Some(tab_view) = widget
        .ancestor(adw::TabView::static_type())
        .and_downcast::<adw::TabView>()
    else {
        return;
    };
    tab_view.close_page(&tab_view.page(&nav));
}

/// The Send button — the composer's primary action. Validates the message,
/// confirms the recipients, then hands the raw document to `git send-email`; on
/// success it toasts and returns the composer to rest.
fn build_send_button(state: &ComposerState, surface: &Surface) -> gtk::Button {
    let button = gtk::Button::builder()
        .label("Send")
        .css_classes(["suggested-action"])
        .build();

    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        surface,
        move |button| {
            let doc = normalize_document(&state.document_text());
            let (headers, _) = message::parse_headers(&doc);

            // Guard the catastrophic edits the raw editor makes possible before
            // handing anything to git.
            if !doc.contains("\n\n") {
                present_message(
                    button,
                    "No Message Body",
                    "Add a blank line after the headers, then your reply below it.",
                );
                return;
            }
            if message::header_value(&headers, "From")
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                present_message(
                    button,
                    "No Sender Configured",
                    "Set your name and email in git (user.name and user.email), \
                     or a sendemail.from, before sending.",
                );
                return;
            }
            if message::header_value(&headers, "To")
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                present_message(button, "No Recipient", "Add a To: header first.");
                return;
            }

            let dialog = adw::AlertDialog::new(Some("Send Reply?"), None);
            dialog.set_body(&confirm_body(&headers));
            dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
            dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
            // Sending is irreversible, so it must be an explicit click: Enter
            // and Escape both cancel rather than fire the default action.
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            dialog.connect_response(
                Some("send"),
                glib::clone!(
                    #[strong]
                    state,
                    #[weak]
                    button,
                    #[strong]
                    surface,
                    move |_, _| {
                        send_now(&state, &button, &surface);
                    }
                ),
            );
            dialog.present(Some(button));
        }
    ));

    button
}

/// Spawn the send on the main loop, disabling the button while it runs. On
/// success: toast, then return the composer to rest (collapse the inline bar,
/// or close the tab). On failure: surface git's diagnostic in a dialog.
fn send_now(state: &ComposerState, button: &gtk::Button, surface: &Surface) {
    let doc = normalize_document(&state.document_text());
    let (headers, _) = message::parse_headers(&doc);
    let request = send::Request {
        from: header_owned(&headers, "From"),
        to: header_owned(&headers, "To"),
        cc: header_owned(&headers, "Cc"),
        eml: doc,
    };
    // Resolve the toast surface now, while the button is still in the tree.
    let overlay = button
        .ancestor(adw::ToastOverlay::static_type())
        .and_downcast::<adw::ToastOverlay>();
    button.set_sensitive(false);

    glib::spawn_future_local(glib::clone!(
        #[strong]
        state,
        #[weak]
        button,
        #[strong]
        surface,
        async move {
            let outcome = send::send(request, &button).await;
            button.set_sensitive(true);
            match outcome {
                Ok(send::Outcome::Sent) => {
                    if let Some(overlay) = &overlay {
                        overlay.add_toast(adw::Toast::new("Reply sent"));
                    }
                    surface.dismiss(&state, &button);
                }
                Ok(send::Outcome::Failed { code, stderr }) => {
                    present_message(
                        &button,
                        "Couldn't Send Reply",
                        &send_error_body(code, &stderr),
                    );
                }
                Err(error) => {
                    present_message(&button, "Couldn't Send Reply", &error.to_string());
                }
            }
        }
    ));
}

/// A header value as an owned, single-line string ("" when absent): any folding
/// is collapsed so a multi-line `Cc`/`To` becomes the clean comma list git wants.
fn header_owned(headers: &[(String, String)], name: &str) -> String {
    message::unfold(message::header_value(headers, name).unwrap_or(""))
}

/// The recipient summary shown in the send confirmation, plus a caution when
/// the reply has lost its threading header.
fn confirm_body(headers: &[(String, String)]) -> String {
    let mut body = format!("To: {}", header_owned(headers, "To"));
    let cc = header_owned(headers, "Cc");
    if !cc.is_empty() {
        body.push_str(&format!("\nCc: {cc}"));
    }
    body.push_str(&format!("\nSubject: {}", header_owned(headers, "Subject")));
    if header_owned(headers, "In-Reply-To").is_empty() {
        body.push_str("\n\nThis reply is not threaded (no In-Reply-To header).");
    }
    body
}

/// A human-facing failure message: git's own stderr when it said anything,
/// otherwise a summary of how it exited.
fn send_error_body(code: Option<i32>, stderr: &str) -> String {
    let stderr = stderr.trim();
    // A missing password — the prompt was cancelled, or none could be obtained —
    // is git's most common failure here; say so plainly instead of showing its
    // raw "terminal prompts disabled" diagnostic.
    if stderr.contains("could not read Password")
        || stderr.contains("terminal prompts disabled")
        || stderr.contains("askpass")
    {
        return "The reply was not sent because no SMTP password was provided. \
                If you cancelled the password prompt, that is expected — nothing was sent, \
                and your draft is untouched."
            .to_string();
    }
    if !stderr.is_empty() {
        // Show the tail, capped, so a verbose failure (the useful part of which
        // is last) cannot produce an unbounded dialog.
        const MAX: usize = 800;
        let count = stderr.chars().count();
        if count > MAX {
            return format!("…{}", stderr.chars().skip(count - MAX).collect::<String>());
        }
        return stderr.to_string();
    }
    match code {
        Some(code) => format!("git send-email exited with status {code}."),
        None => "git send-email was terminated before it finished.".to_string(),
    }
}

/// Show a simple informational dialog with a single Close response.
fn present_message(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

/// A handle to a built composer: the bottom sheet to wrap the page's mail pane
/// in, plus the hooks a per-mail Reply button needs to retarget the draft at
/// another message in the thread.
#[derive(Clone)]
pub struct Composer {
    sheet: adw::BottomSheet,
    state: ComposerState,
    body_view: gtk::TextView,
}

impl Composer {
    /// The composer's `BottomSheet`. The page mounts the mail pane as its
    /// content, so the collapsed bottom bar rests under the thread and the
    /// open editor slides over it.
    pub fn widget(&self) -> &adw::BottomSheet {
        &self.sheet
    }

    /// Point the composer at `reply`: the document is rebuilt against the new
    /// context and the editor is shown and focused. If the user has already
    /// started typing a reply, confirm discarding it first.
    pub fn start_reply(&self, reply: ReplyContext) {
        if self.state.body_is_pristine() {
            self.apply_reply(reply);
            return;
        }

        let dialog = adw::AlertDialog::new(
            Some("Discard Draft?"),
            Some("You have started a reply. Replying to a different message will discard it."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(
            Some("discard"),
            glib::clone!(
                #[strong(rename_to = this)]
                self,
                move |_, _| this.apply_reply(reply.clone())
            ),
        );
        dialog.present(Some(&self.body_view));
    }

    fn apply_reply(&self, reply: ReplyContext) {
        self.state.retarget(reply);
        self.sheet.set_open(true);
        self.body_view.grab_focus();
    }

    /// Insert `quoted` into the body at the cursor (clamped into the body region
    /// so it can never land among the headers) and reveal the editor with the
    /// cursor on the line after the quote.
    pub fn insert_quote(&self, quoted: &str) {
        let buffer = &self.state.document;
        let body_start = body_start_offset(&self.state.document_text());
        let mut iter = buffer.iter_at_mark(&buffer.get_insert());
        if iter.offset() < body_start {
            iter = buffer.iter_at_offset(body_start);
        }
        // Keep the quote on lines of its own: break out of a partially typed
        // line first, and end with a newline so the cursor lands on the line
        // after the quote.
        let mut text = String::new();
        if !iter.starts_line() {
            text.push('\n');
        }
        text.push_str(quoted);
        text.push('\n');
        buffer.insert(&mut iter, &text);
        buffer.place_cursor(&iter);

        self.sheet.set_open(true);
        self.body_view.grab_focus();
        // Bring the cursor into view once the editor has a real allocation: the
        // sheet may only be opening now, and scrolling a view that isn't laid
        // out yet is a no-op.
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

/// Style widgets named `drag-handle` like the sheet's own overlaid handle,
/// and unify the sheet card's two shadows. The declarations are copied
/// verbatim from libadwaita 1.9's `_bottom-sheet.scss`.
///
/// The handle rule drops the stylesheet's sheet-internal scoping
/// (`> stack > widget >`) so the collapsed bottom bar can carry the same
/// pill. The sheet's internal handle matches this selector too; the identical
/// values make that a no-op.
///
/// The shadow rules pin the collapsed bar face (`.bottom-bar`) to the box
/// shadow the open sheet wears instead of the stylesheet's heavier floating
/// glow: the widget switches face 15% into the open swipe, and with the card
/// no longer full-width (whose flush-left/right styling used to suppress the
/// side shadows) the two recipes visibly morphed mid-open. `.hidden` must be
/// restated because this application-priority provider outranks the theme's
/// `.bottom-bar.hidden { box-shadow: none }`.
fn install_handle_style() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            "drag-handle {
                background-color: color-mix(in srgb, currentColor 25%, transparent);
                min-width: 54px;
                min-height: 6px;
                margin: 15px;
                border-radius: 99px;
            }
            bottom-sheet > sheet.bottom-bar {
                box-shadow: 0 0 14px 2px rgb(0 0 6 / 3%),
                            0 0 5px 2px rgb(0 0 6 / 10%),
                            0 0 0 1px rgb(0 0 0 / 5%);
            }
            @media (prefers-contrast: more) {
                bottom-sheet > sheet.bottom-bar {
                    box-shadow: 0 0 14px 2px rgb(0 0 6 / 3%),
                                0 0 5px 2px rgb(0 0 6 / 10%),
                                0 0 0 1px rgb(0 0 0 / 80%);
                }
            }
            bottom-sheet > sheet.bottom-bar.hidden {
                box-shadow: none;
            }",
        );
        let Some(display) = gdk::Display::default() else {
            return;
        };
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// Build the reply composer as a bottom sheet: a drag-handle bottom bar that
/// opens (by click or swipe — the sheet wires that itself) into the
/// raw-message editor sliding up over the thread. Non-modal, so the thread
/// stays readable and interactive while composing: per-mail Reply buttons and
/// Quote in Reply keep working with the editor open. The editor is clamped to
/// the mail page's reading width so it lines up with the message column.
pub fn build_composer(reply: ReplyContext) -> Composer {
    let state = ComposerState::new(reply);

    let (body_editor, body_view) = build_body_editor(&state.document, true);
    highlight::attach(&state.document);
    refresh_highlighting(&state.document);
    state.document.connect_changed(refresh_highlighting);

    // The sheet is built first so the toolbar's dismissing buttons can close it.
    // Not full-width: the sheet card itself keeps the thread's reading width
    // (see the width anchor below) instead of spanning the whole window.
    let sheet = adw::BottomSheet::new();
    sheet.set_modal(false);
    sheet.set_full_width(false);
    // The thread pane keeps a bottom margin the height of the sheet (see
    // thread_page's sheet-height handler), and a non-full-width sheet no
    // longer covers that strip edge to edge: the window background showed
    // through as grey bands flanking the card. Painting the sheet widget
    // itself view-colored fills the strip with the same background as the
    // thread's list view above it.
    sheet.add_css_class("view");
    let surface = Surface::Inline(sheet.clone());

    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();

    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    toolbar.append(&build_insert_button(&state, &root));
    toolbar.append(&build_rewrap_button(&state));
    toolbar.append(&build_open_as_tab_button(&state, &sheet));
    let spacer = gtk::Box::builder().hexpand(true).build();
    toolbar.append(&spacer);
    toolbar.append(&build_discard_button(&state, &surface));
    toolbar.append(&build_send_button(&state, &surface));

    root.append(&toolbar);
    root.append(&body_editor);

    // The open editor keeps the mail column's reading width so its fields line
    // up with the message bodies above it.
    let editor_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&root)
        .build();

    // The sheet's stock drag handle cannot be clicked — it is can-target:
    // false, so clicks pass through it into the sheet. A transparent catcher
    // matching the handle's footprint (54×6 pill plus its 15px margins) sits
    // over the sheet's top center and closes the sheet on click, making the
    // handle behave like the toggle it looks like.
    let handle_target = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .width_request(84)
        .height_request(36)
        .build();
    let handle_click = gtk::GestureClick::new();
    handle_click.connect_released(glib::clone!(
        #[weak]
        sheet,
        move |_, _, _, _| sheet.set_open(false)
    ));
    handle_target.add_controller(handle_click);

    // A non-full-width sheet card is allocated its content's *natural* width
    // (clamped to the window), but the editor's natural width wanders with the
    // scrolled text inside it. This invisible anchor — an empty paintable with
    // an intrinsic width of the mail column's maximum, free to shrink — pins
    // the card's natural width to the thread's reading width without raising
    // its minimum, so narrow windows still get a full-width sheet: the card
    // ends up min(window, 1100) wide and centered, same as the clamps above.
    let width_anchor = gtk::Picture::builder()
        .paintable(&gdk::Paintable::new_empty(1100, 0))
        .can_shrink(true)
        .can_target(false)
        .build();
    let sheet_body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    sheet_body.append(&editor_clamp);
    sheet_body.append(&width_anchor);

    let sheet_content = gtk::Overlay::builder().child(&sheet_body).build();
    sheet_content.add_overlay(&handle_target);
    sheet.set_sheet(Some(&sheet_content));

    // The collapsed face of the composer is nothing but a drag handle, the
    // same pill the open sheet wears (see install_handle_style). The sheet
    // itself renders the bar's background and makes the whole strip clickable
    // and swipable.
    install_handle_style();
    let bar_handle = gtk::Box::builder()
        .css_name("drag-handle")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    sheet.set_bottom_bar(Some(&bar_handle));

    // Opening lands the cursor in the editor, mirroring what start_reply does
    // for per-mail Reply buttons.
    sheet.connect_open_notify(glib::clone!(
        #[weak]
        body_view,
        move |sheet| {
            if sheet.is_open() {
                body_view.grab_focus();
            }
        }
    ));

    Composer {
        sheet,
        state,
        body_view,
    }
}

/// Recolor the composer document on every edit: the quote/diff pass plus the
/// trailing-whitespace flag, which is a composer-only concern (received mail is
/// read-only and left as-is). Neither pass emits "changed", so wiring this as
/// the changed handler does not loop.
fn refresh_highlighting(buffer: &gtk::TextBuffer) {
    highlight::refresh(buffer);
    highlight::mark_trailing_whitespace(buffer);
}

/// A monospace editor over the raw message, with a dim 72-column ruler overlaid
/// at the wrap margin. `compact` limits the height so the sticky bar stays
/// small; the fullscreen dialog passes false and lets the editor fill it.
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
    // avoids accumulating per-char rounding error, and the position must track
    // the horizontal scroll offset: the overlay is fixed in viewport
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
    // On map the pango context carries the real font; the scroll handler keeps
    // the ruler on column 72 as the text pans under the overlay.
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

fn build_insert_button(
    state: &ComposerState,
    action_scope: &impl IsA<gtk::Widget>,
) -> gtk::MenuButton {
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
            let trailer = format!("{kind}: {}", sender_identity());
            state.replace_body(&append_trailer(&state.body_text(), &trailer));
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
                    state.replace_body(&append_signature(&state.body_text(), &signature));
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
            // Rewrap only the body; the headers are never wrapped. The
            // configured signature is passed so a copy at the end of the body
            // survives verbatim rather than being reflowed.
            state.replace_body(&rewrap(
                &state.body_text(),
                WRAP_WIDTH,
                &settings::reply_signature(),
            ));
        }
    ));
    button
}

/// The "Open as a Tab" button: lifts the current draft into a full-page
/// composer in its own tab, then returns the inline composer to rest so the same
/// reply is not open in two places. The window opens the tab (via the enclosing
/// `TabView`, found by walking up from the button); the draft continues there.
fn build_open_as_tab_button(state: &ComposerState, sheet: &adw::BottomSheet) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Open as a Tab")
        .css_classes(["flat"])
        .build();

    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[weak]
        sheet,
        move |button| {
            let Some(tab_view) = button
                .ancestor(adw::TabView::static_type())
                .and_downcast::<adw::TabView>()
            else {
                return;
            };
            // Carry the draft exactly as typed into the new tab, keyed to the
            // same reply so its Discard resets to the same baseline.
            let reply = state.initial.borrow().clone();
            let draft = state.document_text();
            crate::open_composer_in_new_tab(&tab_view, reply, Some(draft));
            // The draft now lives in the tab; close and reset the sheet.
            state.reset();
            sheet.set_open(false);
        }
    ));

    button
}

/// Build the standalone composer as a full-page tab: the composer's tools in a
/// header bar at the top of the page, the raw-message editor filling the rest.
/// `seed` carries a draft over from the inline composer (its current text);
/// without it the page starts from `reply`'s fresh prefill. Sending or
/// discarding closes the tab.
pub fn build_composer_page(reply: ReplyContext, seed: Option<String>) -> adw::NavigationPage {
    let state = ComposerState::new(reply);
    if let Some(doc) = seed {
        state.set_document(&doc);
    }

    let (body_editor, body_view) = build_body_editor(&state.document, false);
    highlight::attach(&state.document);
    refresh_highlighting(&state.document);
    state.document.connect_changed(refresh_highlighting);

    let surface = Surface::Tab;

    // The composer's tools sit in a flat toolbar, not a second header bar:
    // koshi's pages carry no header bar of their own, so the window's is the
    // only one. It mirrors the inline composer's row — composing aids on the
    // left, the destructive/primary actions on the right (Send at the edge).
    let toolbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let insert = build_insert_button(&state, &toolbar);
    toolbar.append(&insert);
    toolbar.append(&build_rewrap_button(&state));
    let spacer = gtk::Box::builder().hexpand(true).build();
    toolbar.append(&spacer);
    toolbar.append(&build_discard_button(&state, &surface));
    toolbar.append(&build_send_button(&state, &surface));

    // Clamp the toolbar to the editor's reading width so its buttons line up
    // with the message column, like everything else on the page.
    let toolbar_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&toolbar)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&body_editor);
    let editor_clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .vexpand(true)
        .child(&content)
        .build();

    // The page carries no header bar (nor a ToolbarView top bar): the toolbar
    // and editor rest on the page background, clamped to the reading width and
    // with no divider between them, so the window keeps a single top bar and the
    // debug stripes don't reach here.
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    root.append(&toolbar_clamp);
    root.append(&editor_clamp);

    let page = adw::NavigationPage::builder()
        .title(page_title(&state))
        .child(&root)
        .build();

    // Land the cursor in the editor whenever the tab is shown.
    body_view.connect_map(|view| {
        view.grab_focus();
    });

    page
}

/// The page/tab title for a standalone composer: the message's Subject, or
/// "New Message" when it has none (a blank compose).
fn page_title(state: &ComposerState) -> String {
    let (headers, _) = message::parse_headers(&state.document_text());
    let subject = message::header_value(&headers, "Subject")
        .unwrap_or("")
        .trim();
    if subject.is_empty() {
        "New Message".to_string()
    } else {
        subject.to_string()
    }
}

fn build_discard_button(state: &ComposerState, surface: &Surface) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Discard")
        .css_classes(["flat"])
        .build();

    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        #[strong]
        surface,
        move |button| {
            // Discarding resets the inline bar to its baseline; on a tab there is
            // nothing to reset to, so it closes the tab.
            let (heading, body) = match surface {
                Surface::Inline(_) => (
                    "Discard Draft?",
                    "The draft will be reset to the original reply",
                ),
                Surface::Tab => (
                    "Discard Message?",
                    "This message will be discarded and its tab closed.",
                ),
            };
            let dialog = adw::AlertDialog::new(Some(heading), Some(body));
            dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard")]);
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            dialog.connect_response(
                Some("discard"),
                glib::clone!(
                    #[strong]
                    state,
                    #[strong]
                    surface,
                    #[weak]
                    button,
                    move |_, _| surface.dismiss(&state, &button)
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
    fn body_start_offset_lands_just_after_the_separator() {
        // The body starts right after the blank line that ends the headers.
        let doc = "From: a@b\nTo: c@d\n\nbody\n";
        let offset = body_start_offset(doc);
        assert_eq!(
            doc.chars().skip(offset as usize).collect::<String>(),
            "body\n"
        );
        // No blank line: the whole document is headers, so the offset is its end.
        assert_eq!(body_start_offset("From: a@b\n"), 10);
    }

    #[test]
    fn rewrap_wraps_long_lines_at_word_boundaries() {
        let input = "one two three ".repeat(10); // 140 chars on one line
        let wrapped = rewrap(input.trim_end(), 72, "");
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
        let wrapped = rewrap(&input, 72, "");
        assert!(wrapped.lines().next().unwrap() == quoted, "quote rewrapped");
        assert!(wrapped.ends_with("A reply line"));
    }

    #[test]
    fn rewrap_preserves_paragraph_breaks() {
        let long = "word ".repeat(30);
        let input = format!("{}\n\n{}", long.trim_end(), "short second paragraph");
        let wrapped = rewrap(&input, 72, "");
        assert!(wrapped.contains("\n\n"), "blank line lost: {wrapped:?}");
        assert!(wrapped.ends_with("short second paragraph"));
        // Consecutive lines of one paragraph are joined before wrapping.
        assert_eq!(
            rewrap("joined\nacross\nlines", 72, ""),
            "joined across lines"
        );
    }

    #[test]
    fn rewrap_keeps_unbreakable_words_whole() {
        let url = format!("https://example.com/{}", "x".repeat(80));
        let input = format!("see {url} for details");
        let wrapped = rewrap(&input, 72, "");
        assert!(
            wrapped.lines().any(|line| line == url),
            "URL broken: {wrapped:?}"
        );
    }

    #[test]
    fn rewrap_preserves_trailing_newline() {
        assert_eq!(rewrap("line\n", 72, ""), "line\n");
        assert_eq!(rewrap("line", 72, ""), "line");
    }

    #[test]
    fn rewrap_is_idempotent() {
        let input = format!(
            "{}\n> quoted line kept verbatim\n\nsecond paragraph {}\n",
            "word ".repeat(40).trim_end(),
            "tail ".repeat(30).trim_end()
        );
        let once = rewrap(&input, 72, "");
        assert_eq!(rewrap(&once, 72, ""), once);
    }

    #[test]
    fn rewrap_preserves_a_diff_verbatim() {
        // A prose intro (long enough to wrap) followed by a git-format-patch
        // body: the whole patch — scissors, diffstat, header, hunk, +/- and
        // context lines — must come through byte-for-byte.
        let intro = "word ".repeat(30);
        let patch = "\
---
 src/frob.c | 3 ++-
 1 file changed, 2 insertions(+), 1 deletion(-)

diff --git a/src/frob.c b/src/frob.c
index 1111111..2222222 100644
--- a/src/frob.c
+++ b/src/frob.c
@@ -1,2 +1,2 @@
 keep this context line long enough that it would wrap if it were ever treated as prose
-old line
+new line";
        let input = format!("{}\n\n{patch}\n", intro.trim_end());
        let wrapped = rewrap(&input, 72, "");
        assert!(wrapped.contains(patch), "patch was mangled:\n{wrapped}");
        // The intro before the patch still wrapped.
        assert!(
            wrapped.lines().take_while(|l| *l != "---").count() > 1,
            "intro not wrapped: {wrapped:?}"
        );
    }

    #[test]
    fn rewrap_preserves_the_configured_signature() {
        // A signature without any "-- " marker, sitting at the end, survives
        // verbatim even though it looks like ordinary prose.
        let signature = "Nika Krasnova\nhttps://nikableh.moe";
        let intro = "word ".repeat(30);
        let input = format!("{}\n\n{signature}\n", intro.trim_end());
        let wrapped = rewrap(&input, 72, signature);
        assert!(
            wrapped.ends_with(&format!("{signature}\n")),
            "signature reflowed: {wrapped:?}"
        );
    }

    #[test]
    fn rewrap_reflows_signature_lookalike_that_is_not_at_the_end() {
        // The same text as the signature, but with prose after it, is not the
        // signature — it must be rewrapped like any other prose.
        let signature = "Nika Krasnova and a long trailing line that would wrap when reflowed here";
        let input = format!("{signature}\n\nmore prose after the block\n");
        let wrapped = rewrap(&input, 72, signature);
        assert!(
            !wrapped.contains(signature),
            "lookalike block was frozen: {wrapped:?}"
        );
        assert!(wrapped.ends_with("more prose after the block\n"));
    }

    #[test]
    fn rewrap_empty_signature_never_freezes() {
        // A deliberate "no signature" must not peel anything off the end.
        let long = "word ".repeat(30);
        let input = format!("{}\n", long.trim_end());
        let wrapped = rewrap(&input, 72, "");
        assert!(wrapped.lines().count() > 1, "body not wrapped: {wrapped:?}");
    }

    #[test]
    fn rewrap_handles_a_patch_and_signature_together() {
        // prose + diff + signature: the diff and the signature are both
        // preserved, and the whole thing is idempotent.
        let intro = "word ".repeat(20);
        let signature = "-- \nNika";
        let input = format!(
            "{}\n\ndiff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n\n{signature}\n",
            intro.trim_end()
        );
        let once = rewrap(&input, 72, signature);
        assert!(once.contains("@@ -1 +1 @@"), "hunk lost: {once}");
        assert!(
            once.ends_with(&format!("{signature}\n")),
            "sig lost: {once}"
        );
        assert_eq!(rewrap(&once, 72, signature), once, "not idempotent");
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
}
