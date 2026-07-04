use adw::prelude::*;
use gtk::{gdk, gio, glib};
use mailparse::MailHeaderMap;

use crate::favorites::{self, Favorite};

const RAW_MAIL: &str = include_str!("../data/sample-mail.txt");

struct Mail {
    subject: String,
    from: String,
    to: String,
    to_addrs: Vec<String>,
    cc: Option<String>,
    cc_addrs: Vec<String>,
    date: String,
    message_id: Option<String>,
    body: String,
}

fn parse_sample_mail() -> Mail {
    // Skip the mbox "From " separator line, which is not an RFC 5322 header.
    let raw = if RAW_MAIL.starts_with("From ") {
        match RAW_MAIL.split_once('\n') {
            Some((_, rest)) => rest,
            None => RAW_MAIL,
        }
    } else {
        RAW_MAIL
    };

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
            body: String::new(),
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
        body,
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

pub fn build_thread_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let mail = parse_sample_mail();

    let overlay = adw::ToastOverlay::new();

    let title = gtk::Label::builder()
        .label(&mail.subject)
        .halign(gtk::Align::Start)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["title-2", "monospace"])
        .build();

    let title_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    title_row.append(&build_star_button(&mail, &overlay));
    title_row.append(&title);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
        .build();
    content.append(&title_row);
    content.append(&build_header_list(&mail, &overlay));
    content.append(&build_body_view(&mail, nav, &overlay));

    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();
    overlay.set_child(Some(&scrolled));

    adw::NavigationPage::new(&overlay, &mail.subject)
}

/// A star toggle sitting left of the subject, aligned with its first line.
/// Disabled when the mail has no Message-ID to key the favorite by.
fn build_star_button(mail: &Mail, overlay: &adw::ToastOverlay) -> gtk::ToggleButton {
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

fn build_header_list(mail: &Mail, overlay: &adw::ToastOverlay) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Gives every field-name label the same width so the values line up in
    // a single column; wrapped value lines then stay indented at that column.
    let titles = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);

    list.append(&build_text_row("Author", &mail.from, &titles));
    list.append(&build_address_row("To", &mail.to, &mail.to_addrs, overlay, &titles));
    if let Some(cc) = &mail.cc {
        list.append(&build_address_row("Cc", cc, &mail.cc_addrs, overlay, &titles));
    }
    list.append(&build_text_row("Date", &mail.date, &titles));

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
        .css_classes(["caption-heading"])
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
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(value)))
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
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
    view: &gtk::TextView,
    nav: &adw::NavigationView,
    overlay: &adw::ToastOverlay,
) -> [gio::SimpleAction; 4] {
    let open_web = gio::SimpleAction::new("open-web", None);
    let lore_url = mail.message_id.as_deref().map(|id| {
        let bare = id.trim().trim_start_matches('<').trim_end_matches('>');
        format!("https://lore.kernel.org/r/{bare}/")
    });
    open_web.set_enabled(lore_url.is_some());
    open_web.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| {
            if let Some(url) = &lore_url {
                launch_uri(&view, url);
            }
        }
    ));

    let copy_id = gio::SimpleAction::new("copy-message-id", None);
    let message_id = mail.message_id.clone();
    copy_id.set_enabled(message_id.is_some());
    copy_id.connect_activate(glib::clone!(
        #[weak]
        view,
        #[weak]
        overlay,
        move |_, _| {
            if let Some(id) = &message_id {
                view.clipboard().set_text(id);
                overlay.add_toast(adw::Toast::new("Message-ID copied"));
            }
        }
    ));

    let raw = gio::SimpleAction::new("raw", None);
    raw.connect_activate(glib::clone!(
        #[weak]
        nav,
        move |_, _| nav.push(&build_raw_page())
    ));

    let reply = gio::SimpleAction::new("reply", None);
    let mailto = build_reply_mailto(mail);
    reply.connect_activate(glib::clone!(
        #[weak]
        view,
        move |_, _| launch_uri(&view, &mailto)
    ));

    [open_web, copy_id, raw, reply]
}

fn build_reply_mailto(mail: &Mail) -> String {
    let escape = |s: &str| glib::Uri::escape_string(s, None, false);

    let subject = if mail.subject.to_lowercase().starts_with("re:") {
        mail.subject.clone()
    } else {
        format!("Re: {}", mail.subject)
    };

    let cc = match &mail.cc {
        Some(cc) => format!("{}, {}", mail.to, cc),
        None => mail.to.clone(),
    };

    let mut uri = format!(
        "mailto:{}?cc={}&subject={}",
        escape(&mail.from),
        escape(&cc),
        escape(&subject),
    );
    if let Some(id) = &mail.message_id {
        uri.push_str("&In-Reply-To=");
        uri.push_str(&escape(id));
    }
    uri
}

fn build_raw_page() -> adw::NavigationPage {
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    view.buffer().set_text(RAW_MAIL);

    let scrolled = gtk::ScrolledWindow::builder().child(&view).build();
    adw::NavigationPage::new(&scrolled, "Raw")
}

fn build_body_view(
    mail: &Mail,
    nav: &adw::NavigationView,
    overlay: &adw::ToastOverlay,
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

    let copy_quoted = |prefix: Option<String>| {
        glib::clone!(
            #[weak]
            view,
            #[weak]
            overlay,
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
                    view.clipboard().set_text(&result);
                    overlay.add_toast(adw::Toast::new("Quoted text copied"));
                }
            }
        )
    };

    let quote = gio::SimpleAction::new("quote-selection", None);
    quote.set_enabled(false);
    quote.connect_activate(copy_quoted(None));

    let quote_with_date = gio::SimpleAction::new("quote-with-date", None);
    quote_with_date.set_enabled(false);
    quote_with_date.connect_activate(copy_quoted(Some(format!(
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
    for action in build_mail_actions(mail, &view, nav, overlay) {
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
    mail_section.append(Some("Open on _Web"), Some("mailview.open-web"));
    mail_section.append(Some("Copy _Message-ID"), Some("mailview.copy-message-id"));
    mail_section.append(Some("View _Raw"), Some("mailview.raw"));
    mail_section.append(Some("_Reply"), Some("mailview.reply"));

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
    gtk::UriLauncher::new(uri).launch(
        parent.as_ref(),
        gio::Cancellable::NONE,
        |result| {
            if let Err(error) = result {
                eprintln!("Failed to launch URI: {error}");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_mail_headers() {
        let mail = parse_sample_mail();
        assert_eq!(mail.from, "Rosen Penev <rosenp@gmail.com>");
        assert_eq!(mail.to, "linux-scsi@vger.kernel.org");
        assert_eq!(mail.subject, "[PATCHv2] scsi: st: use kzalloc_array()");
        assert_eq!(
            mail.message_id.as_deref(),
            Some("<20260703215345.253901-1-rosenp@gmail.com>")
        );
        // RFC 2047 encoded word must be decoded.
        let cc = mail.cc.as_deref().unwrap();
        assert!(cc.contains("Kai Mäkisara"), "Cc not decoded: {cc}");
        assert!(!mail.body.is_empty());
        assert!(mail.body.starts_with("Merge allocations"));
    }

    #[test]
    fn parses_address_lists_cleanly() {
        let mail = parse_sample_mail();
        assert_eq!(mail.to_addrs, vec!["linux-scsi@vger.kernel.org"]);

        assert_eq!(mail.cc_addrs.len(), 7, "Cc: {:?}", mail.cc_addrs);
        assert_eq!(mail.cc_addrs[0], "Kai Mäkisara <Kai.Makisara@kolumbus.fi>");
        // RFC 5322 comments (including the nested-paren MAINTAINERS-style
        // one) must be stripped from the parsed addresses.
        for addr in &mail.cc_addrs {
            assert!(!addr.contains("(open list"), "comment leaked: {addr}");
            assert!(!addr.contains("__counted_by"), "comment leaked: {addr}");
        }
        assert!(mail.cc_addrs.contains(&"linux-kernel@vger.kernel.org".to_string()));
        assert!(mail.cc_addrs.contains(&"linux-hardening@vger.kernel.org".to_string()));
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
    fn reply_mailto_is_percent_encoded() {
        let mail = parse_sample_mail();
        let uri = build_reply_mailto(&mail);
        assert!(uri.starts_with("mailto:Rosen%20Penev%20%3Crosenp%40gmail.com%3E?"));
        assert!(uri.contains("subject=Re%3A%20%5BPATCHv2%5D"));
        assert!(uri.contains("&In-Reply-To=%3C20260703215345.253901-1-rosenp%40gmail.com%3E"));
        // Raw header-breaking characters must never appear unencoded.
        let query = uri.split_once('?').unwrap().1;
        assert!(!query.contains(['<', '>', ' ']));
    }
}
