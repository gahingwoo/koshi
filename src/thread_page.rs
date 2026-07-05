use std::collections::HashMap;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use mailparse::MailHeaderMap;

use crate::composer;
use crate::favorites::{self, Favorite};
use crate::highlight;
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

/// Parse an mboxrd thread into its messages, in file order (lore serves
/// `t.mbox.gz` already in thread order). mboxrd ">From " escaping is undone
/// on the raw message text before MIME parsing: the mbox writer escapes raw
/// file lines, so unescaping must happen before any Content-Transfer-Encoding
/// decoding, not after.
fn parse_thread(mbox: &str) -> Vec<Mail> {
    split_mbox(mbox)
        .iter()
        .map(|raw| parse_message(&unescape_mboxrd(raw)))
        .collect()
}

/// Split an mboxrd file into raw messages on its "From " separator lines.
/// Body lines starting with "From " are ">"-escaped in mboxrd, so a line
/// beginning "From " at column zero is always a separator. The separator
/// lines themselves are dropped; anything before the first one is too.
fn split_mbox(mbox: &str) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in mbox.lines() {
        if line.starts_with("From ") {
            messages.extend(current.take());
            current = Some(String::new());
        } else if let Some(message) = current.as_mut() {
            message.push_str(line);
            message.push('\n');
        }
    }
    messages.extend(current);
    messages
}

/// Undo mboxrd body escaping: any line of one-or-more '>' followed by
/// "From " loses one leading '>'.
fn unescape_mboxrd(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        let quoted = line.trim_start_matches('>');
        if quoted.starts_with("From ") && quoted.len() < line.len() {
            out.push_str(&line[1..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !body.ends_with('\n') {
        out.pop();
    }
    out
}

fn parse_message(raw: &str) -> Mail {
    let unknown = || "(unknown)".to_string();
    let Ok(parsed) = mailparse::parse_mail(raw.as_bytes()) else {
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
            raw: raw.to_string(),
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
        raw: raw.to_string(),
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
            return part.get_body().ok();
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
    let split = adw::OverlaySplitView::builder()
        .content(remote.widget())
        .sidebar_position(gtk::PackType::End)
        .show_sidebar(false)
        .sidebar_width_fraction(0.66)
        .min_sidebar_width(360.0)
        .max_sidebar_width(1400.0)
        .build();
    let page = adw::NavigationPage::new(&split, "Loading…");
    page.set_widget_name(THREAD_PAGE_NAME);
    spawn_thread_load(remote, split, nav.clone(), page.clone(), list.to_string(), message_id.to_string());
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
                let thread = parse_thread(&mbox);
                if thread.is_empty() {
                    let error = lore::Error::Parse("the thread has no messages".to_string());
                    show_thread_error(&remote, &error, &split, &nav, &page, list, message_id);
                } else {
                    page.set_title(&thread[0].subject);
                    let widgets = build_thread_content(&nav, &thread, &list);
                    remote.show_content(&widgets.content);
                    split.set_sidebar(Some(&widgets.overview));
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
    let weak = remote.downgrade();
    let split = split.downgrade();
    let nav = nav.downgrade();
    let page = page.downgrade();
    remote.show_error(error, move || {
        let (Some(remote), Some(split), Some(nav), Some(page)) =
            (weak.upgrade(), split.upgrade(), nav.upgrade(), page.upgrade())
        else {
            return;
        };
        spawn_thread_load(remote, split, nav, page, list.clone(), message_id.clone());
    });
}

/// The two widgets a loaded thread produces: the scrolling message stack
/// (with the composer below it) and the overview sidebar for the split view.
struct ThreadWidgets {
    content: gtk::Box,
    overview: gtk::Widget,
}

fn build_thread_content(
    nav: &adw::NavigationView,
    thread: &[Mail],
    list: &str,
) -> ThreadWidgets {
    let op = &thread[0];

    let overlay = adw::ToastOverlay::new();
    // The composer opens targeting the OP; each mail's Reply button can
    // retarget it later.
    let composer = composer::build_composer(build_reply_context(op));

    // Selectable but not focusable: see build_text_row for why.
    let title = gtk::Label::builder()
        .label(&op.subject)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .selectable(true)
        .focusable(false)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["title-2", "monospace"])
        .build();

    // The OP's Reply button lives up here next to the star rather than in
    // its header list, aligned with the title's first line like the star.
    let op_reply = build_reply_button(op, &composer);
    op_reply.set_valign(gtk::Align::Start);

    let title_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    title_row.append(&title);
    title_row.append(&build_star_button(op, list, &overlay));
    title_row.append(&op_reply);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(24)
        .build();
    content.append(&title_row);
    // One title-column width shared by every card, so the header value
    // columns line up across the whole stack, not just within one message.
    let titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
    let mut sections: Vec<gtk::Box> = Vec::with_capacity(thread.len());
    for (index, mail) in thread.iter().enumerate() {
        let is_op = index == 0;
        let section = build_message_section(mail, is_op, nav, &overlay, &composer, &titles);
        content.append(&section);
        sections.push(section);
    }

    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .vexpand(true)
        .build();
    // The ScrolledWindow wraps the clamp in a GtkViewport whose
    // scroll-to-focus behavior jumps the page whenever a child grabs focus —
    // e.g. clicking into a message body to select text. That auto-scroll is
    // exactly right for keyboard focus (Tab must bring the focused widget
    // into view), so instead of turning it off wholesale, suppress it only
    // around pointer clicks: disable on press (capture phase, before the
    // click's focus grab) and re-enable from an idle once the grab has been
    // processed.
    if let Some(viewport) = scrolled.child().and_downcast::<gtk::Viewport>() {
        let gesture = gtk::GestureClick::builder()
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        gesture.connect_pressed(glib::clone!(
            #[weak]
            viewport,
            move |_, _, _, _| {
                viewport.set_scroll_to_focus(false);
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    viewport,
                    move || viewport.set_scroll_to_focus(true)
                ));
            }
        ));
        scrolled.add_controller(gesture);
    }
    overlay.set_child(Some(&scrolled));

    // The composer sits below the scrolling mail body in a plain box, so it
    // stays visible without living in a ToolbarView bottom bar (whose
    // GtkWindowHandle wrapper would turn clicks and drags on the composer
    // padding into window move/maximize gestures).
    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_box.append(&overlay);
    content_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content_box.append(composer.widget());

    ThreadWidgets {
        content: content_box,
        overview: build_overview_sidebar(thread, sections, &scrolled),
    }
}

/// One row of the overview tree: which message, how deep, and who it
/// replies to (None for messages that start their own subthread).
struct TreeRow {
    index: usize,
    depth: usize,
    parent: Option<usize>,
}

/// Arrange the thread as lore.kernel.org's overview does: depth-first over
/// the In-Reply-To graph, children in arrival order. A message whose parent
/// is missing from the thread starts at depth zero; a parent may well appear
/// later in the mbox than its reply (lore serves cover letters after the
/// first patch), so linking is by id, not by file position.
fn thread_tree(thread: &[Mail]) -> Vec<TreeRow> {
    let mut position: HashMap<&str, usize> = HashMap::new();
    for (index, mail) in thread.iter().enumerate() {
        if let Some(id) = &mail.message_id {
            position.entry(normalize_message_id(id)).or_insert(index);
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); thread.len()];
    let mut roots: Vec<usize> = Vec::new();
    for (index, mail) in thread.iter().enumerate() {
        let parent = mail
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

    let mut rows = Vec::with_capacity(thread.len());
    let mut emitted = vec![false; thread.len()];
    let mut stack: Vec<TreeRow> = roots
        .iter()
        .rev()
        .map(|&index| TreeRow { index, depth: 0, parent: None })
        .collect();
    loop {
        while let Some(row) = stack.pop() {
            // The emitted guard makes reference cycles finite: a child that
            // was already written out is not descended into again.
            if std::mem::replace(&mut emitted[row.index], true) {
                continue;
            }
            let (index, depth) = (row.index, row.depth);
            rows.push(row);
            stack.extend(children[index].iter().rev().map(|&child| TreeRow {
                index: child,
                depth: depth + 1,
                parent: Some(index),
            }));
        }
        // Messages caught in a reference cycle have no root to be reached
        // from; surface the first stranded one as a root and keep going.
        match emitted.iter().position(|&done| !done) {
            Some(index) => stack.push(TreeRow { index, depth: 0, parent: None }),
            None => return rows,
        }
    }
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

/// Subject with any leading "Re:" chains removed, for telling "same
/// conversation" apart from "subject actually changed".
fn strip_reply_prefixes(subject: &str) -> &str {
    let mut subject = subject.trim();
    while let Some(rest) = subject
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("re:"))
        .map(|_| subject[3..].trim_start())
    {
        subject = rest;
    }
    subject
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

/// One overview line, mirroring lore: date, indent, a "`" reply marker,
/// then the subject (only when it differs from the parent's, ignoring Re:)
/// and the author.
fn overview_row_label(mail: &Mail, depth: usize, parent_subject: Option<&str>) -> String {
    let mut label = format!("{} ", overview_date(&mail.date));
    if depth > 0 {
        label.push_str(&"  ".repeat(depth - 1));
        label.push_str("` ");
    }
    let same_subject = parent_subject.is_some_and(|parent| {
        strip_reply_prefixes(parent).eq_ignore_ascii_case(strip_reply_prefixes(&mail.subject))
    });
    if parent_subject.is_none() || same_subject {
        label.push_str(author_name(&mail.from));
    } else {
        label.push_str(mail.subject.trim());
        label.push(' ');
        label.push_str(author_name(&mail.from));
    }
    label
}

/// The overview sidebar: a heading over one activatable row per message,
/// laid out as the lore-style tree; activating a row scrolls the message
/// stack to that message.
fn build_overview_sidebar(
    thread: &[Mail],
    sections: Vec<gtk::Box>,
    scrolled: &gtk::ScrolledWindow,
) -> gtk::Widget {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["navigation-sidebar"])
        .build();

    // The list rows are appended in tree order, so their list positions no
    // longer match message order; this maps row position -> message index.
    let mut message_of_row: Vec<usize> = Vec::with_capacity(thread.len());
    for row in thread_tree(thread) {
        // The first message never shows its subject (it heads the page
        // already); a later parentless message compares against the OP so a
        // changed subject still shows up.
        let parent_subject = row
            .parent
            .map(|parent| thread[parent].subject.as_str())
            .or_else(|| (row.index != 0).then(|| thread[0].subject.as_str()));
        let text = overview_row_label(&thread[row.index], row.depth, parent_subject);

        let label = gtk::Label::builder()
            .label(&text)
            .tooltip_text(&text)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .css_classes(["monospace", "caption"])
            .build();
        list.append(&gtk::ListBoxRow::builder().child(&label).build());
        message_of_row.push(row.index);
    }

    list.connect_row_activated(glib::clone!(
        #[weak]
        scrolled,
        move |_, row| {
            let Some(section) = message_of_row
                .get(row.index() as usize)
                .and_then(|&index| sections.get(index))
            else {
                return;
            };
            if let Some(viewport) = scrolled.child().and_downcast::<gtk::Viewport>() {
                viewport.scroll_to(section, None);
            }
        }
    ));

    let heading = gtk::Label::builder()
        .label(format!(
            "Thread Overview — {} message{}",
            thread.len(),
            if thread.len() == 1 { "" } else { "s" }
        ))
        .xalign(0.0)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["heading"])
        .build();

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.append(&heading);
    sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build(),
    );
    sidebar.upcast()
}

/// One message of the thread: its header list stacked over its body view.
fn build_message_section(
    mail: &Mail,
    is_op: bool,
    nav: &adw::NavigationView,
    overlay: &adw::ToastOverlay,
    composer: &composer::Composer,
    titles: &gtk::SizeGroup,
) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    section.append(&build_header_list(mail, is_op, overlay, composer, titles));
    section.append(&build_body_view(mail, nav, composer));
    setup_section_context_menu(mail, &section, nav, composer);
    section
}

// The body view's context menu only covers the body text; this one catches
// right-clicks on the rest of the message card (header rows, padding) so the
// mail actions are reachable from anywhere on the message. The body gesture
// claims its clicks in the capture phase, so this bubble-phase gesture never
// fires for them.
fn setup_section_context_menu(
    mail: &Mail,
    section: &gtk::Box,
    nav: &adw::NavigationView,
    composer: &composer::Composer,
) {
    let group = gio::SimpleActionGroup::new();
    for action in build_mail_actions(mail, section.upcast_ref(), nav, composer) {
        group.add_action(&action);
    }
    section.insert_action_group("mail", Some(&group));

    let menu = gio::Menu::new();
    menu.append(Some("_Reply"), Some("mail.reply"));
    menu.append(Some("Open on _Web"), Some("mail.open-web"));
    menu.append(Some("View _Raw"), Some("mail.raw"));

    // Selectable labels (header values, address pills) pop their own stock
    // menu on right-click, which would otherwise shadow the mail actions;
    // append them there as an extra-menu section.
    add_label_extra_menus(section.upcast_ref(), &menu);

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(section);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    section.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    gesture.connect_pressed(glib::clone!(
        #[weak]
        popover,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    section.add_controller(gesture);
}

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

/// A star toggle sitting right of the subject, aligned with its first line.
/// Disabled when the mail has no Message-ID to key the favorite by.
fn build_star_button(mail: &Mail, list: &str, overlay: &adw::ToastOverlay) -> gtk::ToggleButton {
    let starred = mail
        .message_id
        .as_deref()
        .is_some_and(favorites::is_favorite);

    let button = gtk::ToggleButton::builder()
        .active(starred)
        .valign(gtk::Align::Start)
        .sensitive(mail.message_id.is_some())
        .css_classes(["flat"])
        .build();

    let apply = |button: &gtk::ToggleButton, starred: bool| {
        button.set_icon_name(if starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        button.set_tooltip_text(Some(if starred {
            "Remove from Favorites"
        } else {
            "Add to Favorites"
        }));
    };
    apply(&button, starred);

    let fav = mail.message_id.as_ref().map(|id| Favorite {
        message_id: id.clone(),
        subject: mail.subject.clone(),
        date: mail.date.clone(),
        list: list.to_string(),
    });
    button.connect_toggled(glib::clone!(
        #[weak]
        overlay,
        move |button| {
            let Some(fav) = &fav else { return };
            // Drive the store from the button's own state rather than blindly
            // flipping it: another view of the same mail may have changed the
            // store since this page was built, and a blind flip would then
            // do the opposite of what the click asked for.
            let starred = button.is_active();
            if favorites::is_favorite(&fav.message_id) != starred {
                favorites::toggle(fav.clone());
            }
            apply(button, starred);
            overlay.add_toast(adw::Toast::new(if starred {
                "Added to Favorites"
            } else {
                "Removed from Favorites"
            }));
        }
    ));

    button
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

fn build_header_list(
    mail: &Mail,
    is_op: bool,
    overlay: &adw::ToastOverlay,
    composer: &composer::Composer,
    // Gives every field-name label the same width so the values line up in
    // a single column; wrapped value lines then stay indented at that column.
    titles: &gtk::SizeGroup,
) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // A reply's Subject row doubles as its toolbar: the Reply button trails
    // the hexpanding value label. The OP's subject already heads the page as
    // its title, so its Subject row is omitted and its Reply button sits in
    // the title row instead.
    if !is_op {
        let subject_row = build_text_row("Subject", &mail.subject, titles);
        if let Some(content) = subject_row.child().and_downcast::<gtk::Box>() {
            content.append(&build_reply_button(mail, composer));
        }
        list.append(&subject_row);
    }

    // The author stays on one line no matter how long the display name is.
    list.append(&build_single_line_row("Author", &mail.from, titles));
    list.append(&build_text_row("Date", &mail.date, titles));

    // The remaining headers are collapsed by default: recipients are almost
    // always the same as the OP's and the ids only matter for debugging, so
    // the Message-Id/In-Reply-To/To/Cc rows only show up on request.
    let details = adw::ExpanderRow::builder().title("Details").build();
    if let Some(id) = &mail.message_id {
        details.add_row(&build_text_row("Message-Id", id, titles));
    }
    if let Some(id) = &mail.in_reply_to {
        details.add_row(&build_text_row("In-Reply-To", id, titles));
    }
    details.add_row(&build_address_row("To", &mail.to, &mail.to_addrs, overlay, titles));
    if let Some(cc) = &mail.cc {
        details.add_row(&build_address_row("Cc", cc, &mail.cc_addrs, overlay, titles));
    }
    list.append(&details);

    list
}

/// A non-activatable row laying the field name and its value out on one
/// line: [title | value], with the title column width shared via `titles`.
fn build_row(
    name: &str,
    value: &impl IsA<gtk::Widget>,
    title_valign: gtk::Align,
    titles: &gtk::SizeGroup,
) -> gtk::ListBoxRow {
    // Top-aligned titles (wrapping chip rows) get nudged onto the first
    // value line; centered ones need no offset.
    let title_margin_top = if title_valign == gtk::Align::Start { 6 } else { 0 };
    let title = gtk::Label::builder()
        .label(name)
        .halign(gtk::Align::Start)
        .valign(title_valign)
        .margin_top(title_margin_top)
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    titles.add_widget(&title);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
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

fn build_text_row(name: &str, value: &str, titles: &gtk::SizeGroup) -> gtk::ListBoxRow {
    // A wrapping label's natural width is far narrower than its full text,
    // so it must fill its allocation (halign Fill, the default) — with
    // halign Start it would shrink to that natural width and wrap long
    // before running out of row space. xalign keeps the text left-aligned.
    //
    // Selectable labels are also made non-focusable: GtkLabel draws a text
    // caret whenever a selectable label has key focus and its selection is
    // empty (gtk_label_snapshot), and clicking a selectable label grabs
    // focus — so a plain click leaves a caret behind. Refusing focus removes
    // the caret; the click/drag selection gestures never check focus, so
    // mouse selection and the context-menu Copy keep working.
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(value)))
        .selectable(true)
        .focusable(false)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    build_row(name, &label, gtk::Align::Center, titles)
}

/// Like a text row, but the value never wraps: overlong values (author
/// display names, long addresses) ellipsize instead of growing the row.
fn build_single_line_row(name: &str, value: &str, titles: &gtk::SizeGroup) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(value)))
        .selectable(true)
        .focusable(false)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    build_row(name, &label, gtk::Align::Center, titles)
}

/// Address rows show parsed pills; if parsing produced nothing but the raw
/// header exists, fall back to a plain text row so the value isn't lost.
fn build_address_row(
    name: &str,
    raw: &str,
    addrs: &[String],
    overlay: &adw::ToastOverlay,
    titles: &gtk::SizeGroup,
) -> gtk::ListBoxRow {
    if addrs.is_empty() {
        return build_text_row(name, raw, titles);
    }

    let wrap = adw::WrapBox::builder()
        .child_spacing(6)
        .line_spacing(6)
        .build();
    for addr in addrs {
        wrap.append(&build_address_pill(addr, overlay));
    }

    let row = build_row(name, &wrap, gtk::Align::Start, titles);
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
    nav: &adw::NavigationView,
    composer: &composer::Composer,
) -> [gio::SimpleAction; 3] {
    let reply = gio::SimpleAction::new("reply", None);
    let reply_context = build_reply_context(mail);
    reply.connect_activate(glib::clone!(
        #[strong]
        composer,
        move |_, _| composer.start_reply(reply_context.clone())
    ));

    let open_web = gio::SimpleAction::new("open-web", None);
    let lore_url = mail.message_id.as_deref().map(|id| {
        let bare = id.trim().trim_start_matches('<').trim_end_matches('>');
        format!("https://lore.kernel.org/r/{bare}/")
    });
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

    let raw = gio::SimpleAction::new("raw", None);
    let raw_text = mail.raw.clone();
    let subject = mail.subject.clone();
    raw.connect_activate(glib::clone!(
        #[weak]
        nav,
        move |_, _| nav.push(&build_raw_page(&raw_text, &subject))
    ));

    [reply, open_web, raw]
}

fn build_raw_page(raw: &str, subject: &str) -> adw::NavigationPage {
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

fn build_body_view(
    mail: &Mail,
    nav: &adw::NavigationView,
    composer: &composer::Composer,
) -> adw::Bin {
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
    view.buffer().set_text(&mail.body);
    highlight::attach(&view.buffer());
    highlight::refresh(&view.buffer());

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

    view.buffer().connect_has_selection_notify(glib::clone!(
        #[weak]
        quote,
        #[weak]
        quote_with_date,
        #[weak]
        copy,
        move |buffer| {
            quote.set_enabled(buffer.has_selection());
            quote_with_date.set_enabled(buffer.has_selection());
            copy.set_enabled(buffer.has_selection());
        }
    ));

    // The popover can't be parented to the TextView itself (it allocates its
    // own children and warns about foreign ones), so everything hangs off a
    // plain Box wrapper instead — including the action group.
    let hscroll = gtk::ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();

    let wrapper = adw::Bin::builder().child(&hscroll).build();

    let group = gio::SimpleActionGroup::new();
    group.add_action(&quote);
    group.add_action(&quote_with_date);
    group.add_action(&copy);
    group.add_action(&select_all);
    for action in build_mail_actions(mail, view.upcast_ref(), nav, composer) {
        group.add_action(&action);
    }
    wrapper.insert_action_group("mailview", Some(&group));

    setup_context_menu(&view, &wrapper);

    wrapper
}

// GTK only appends extra-menu items after the built-in ones, so to put the
// quote items first the context menu is replaced wholesale.
fn setup_context_menu(view: &gtk::TextView, wrapper: &adw::Bin) {
    let quote_section = gio::Menu::new();
    quote_section.append(Some("_Quote Selection"), Some("mailview.quote-selection"));
    quote_section.append(Some("Quote With _Date"), Some("mailview.quote-with-date"));

    let edit_section = gio::Menu::new();
    edit_section.append(Some("_Copy"), Some("mailview.copy"));
    edit_section.append(Some("Select _All"), Some("mailview.select-all"));

    let mail_section = gio::Menu::new();
    mail_section.append(Some("_Reply"), Some("mailview.reply"));
    mail_section.append(Some("Open on _Web"), Some("mailview.open-web"));
    mail_section.append(Some("View _Raw"), Some("mailview.raw"));

    let menu = gio::Menu::new();
    menu.append_section(None, &quote_section);
    menu.append_section(None, &edit_section);
    menu.append_section(None, &mail_section);

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(wrapper);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    wrapper.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    gesture.connect_pressed(glib::clone!(
        #[weak]
        popover,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    view.add_controller(gesture);
}

fn launch_uri(widget: &impl IsA<gtk::Widget>, uri: &str) {
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
    const RAW_THREAD: &str = include_str!("../data/sample-thread.mbox");

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
        let mbox = "From a@b Thu Jan  1 00:00:00 1970\nSubject: x\n\n>From escaped\n\
                    From c@d Thu Jan  1 00:00:00 1970\nSubject: y\n\nbody\n";
        let messages = split_mbox(mbox);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].starts_with("Subject: x"));
        assert!(messages[0].contains(">From escaped"));
        assert!(messages[1].starts_with("Subject: y"));
    }

    #[test]
    fn unescapes_mboxrd_from_lines() {
        let body = ">From here\n>>From nested\n> From untouched\nno From here\n";
        assert_eq!(
            unescape_mboxrd(body),
            "From here\n>From nested\n> From untouched\nno From here\n"
        );
        // Trailing-newline shape is preserved.
        assert_eq!(unescape_mboxrd(">From x"), "From x");
    }

    #[test]
    fn unescapes_raw_text_before_transfer_decoding() {
        // Writer-escaped ">From " must lose its '>' before qp decoding,
        // while a '>' the qp decoding itself produces ("=3EFrom") was never
        // escaped by the writer and must survive untouched.
        let raw = "Subject: qp\nContent-Transfer-Encoding: quoted-printable\n\n\
                   >From escaped by the writer\n=3EFrom decoded, stays quoted\n";
        // Compared line-wise: mailparse emits CRLF for decoded qp bodies.
        let mail = parse_message(&unescape_mboxrd(raw));
        let mut lines = mail.body.lines();
        assert_eq!(lines.next(), Some("From escaped by the writer"));
        assert_eq!(lines.next(), Some(">From decoded, stays quoted"));
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
        // The cycle is cut once: its first message roots it, the other nests.
        let late = rows.iter().find(|row| row.index == 3).unwrap();
        assert_eq!((late.parent, late.depth), (Some(2), 1));
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
    fn overview_labels_follow_lore_conventions() {
        let op = mail("op@x", None, "[PATCH 0/2] series", "Nika Krasnova <nika@x>");
        let same = mail("a@x", Some("op@x"), "Re: [PATCH 0/2] series", "Miguel Ojeda <m@x>");
        let changed = mail("b@x", Some("op@x"), "[PATCH 1/2] first patch", "Nika Krasnova <nika@x>");

        // The OP row: date and author only.
        assert_eq!(overview_row_label(&op, 0, None), "2026-06-29 03:51 Nika Krasnova");
        // Same subject as the parent (modulo Re:): author only, marker indented.
        assert_eq!(
            overview_row_label(&same, 1, Some("[PATCH 0/2] series")),
            "2026-06-29 03:51 ` Miguel Ojeda"
        );
        assert_eq!(
            overview_row_label(&same, 3, Some("[PATCH 0/2] series")),
            "2026-06-29 03:51     ` Miguel Ojeda"
        );
        // Changed subject: shown before the author.
        assert_eq!(
            overview_row_label(&changed, 1, Some("[PATCH 0/2] series")),
            "2026-06-29 03:51 ` [PATCH 1/2] first patch Nika Krasnova"
        );
    }

    #[test]
    fn overview_dates_fall_back_verbatim() {
        let mut broken = mail("x@x", None, "s", "A");
        broken.date = "not a date".to_string();
        assert_eq!(overview_date(&broken.date), "not a date");
    }

    #[test]
    fn normalizes_message_id_references() {
        assert_eq!(normalize_message_id("<a@b>"), "a@b");
        assert_eq!(normalize_message_id(" <a@b> <c@d>"), "a@b");
        assert_eq!(normalize_message_id("bare@id "), "bare@id");
        assert_eq!(strip_reply_prefixes("Re: RE: re:subject"), "subject");
        assert_eq!(strip_reply_prefixes("Regarding x"), "Regarding x");
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
}
