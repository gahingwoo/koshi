use adw::prelude::*;
use gtk::{gdk, gio, glib};
use mailparse::MailHeaderMap;

const RAW_MAIL: &str = include_str!("../data/sample-mail.txt");

struct Mail {
    subject: String,
    from: String,
    to: String,
    cc: Option<String>,
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
            cc: None,
            date: unknown(),
            message_id: None,
            body: String::new(),
        };
    };

    let header = |name: &str| parsed.headers.get_first_value(name);
    let body = find_text_body(&parsed).unwrap_or_default();

    Mail {
        subject: header("Subject").unwrap_or_else(unknown),
        from: header("From").unwrap_or_else(unknown),
        to: header("To").unwrap_or_else(unknown),
        cc: header("Cc"),
        date: header("Date").unwrap_or_else(unknown),
        message_id: header("Message-ID"),
        body,
    }
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
        .css_classes(["title-2"])
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .spacing(12)
        .build();
    content.append(&title);
    content.append(&build_header_list(&mail));
    content.append(&build_action_buttons(&mail, nav, &overlay));
    content.append(&build_body_view(&mail, &overlay));

    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .tightening_threshold(800)
        .child(&content)
        .build();

    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();
    overlay.set_child(Some(&scrolled));

    adw::NavigationPage::new(&overlay, &mail.subject)
}

fn build_header_list(mail: &Mail) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    list.append(&build_header_row("Author", &mail.from));
    list.append(&build_header_row("To", &mail.to));
    if let Some(cc) = &mail.cc {
        list.append(&build_header_row("Cc", cc));
    }
    list.append(&build_header_row("Date", &mail.date));

    list
}

fn build_header_row(name: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(name))
        .subtitle(glib::markup_escape_text(value))
        .subtitle_lines(0)
        .activatable(false)
        .css_classes(["property"])
        .build();
    row.set_subtitle_selectable(true);
    row
}

fn build_action_buttons(
    mail: &Mail,
    nav: &adw::NavigationView,
    overlay: &adw::ToastOverlay,
) -> gtk::Box {
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .build();

    let open_web = gtk::Button::builder()
        .icon_name("web-browser-symbolic")
        .tooltip_text("Open on Web")
        .css_classes(["flat"])
        .build();
    let lore_url = mail.message_id.as_deref().map(|id| {
        let bare = id.trim().trim_start_matches('<').trim_end_matches('>');
        format!("https://lore.kernel.org/r/{bare}/")
    });
    open_web.set_sensitive(lore_url.is_some());
    open_web.connect_clicked(move |button| {
        if let Some(url) = &lore_url {
            launch_uri(button, url);
        }
    });
    buttons.append(&open_web);

    let copy_id = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy Message-ID")
        .css_classes(["flat"])
        .build();
    let message_id = mail.message_id.clone();
    copy_id.set_sensitive(message_id.is_some());
    copy_id.connect_clicked(glib::clone!(
        #[weak]
        overlay,
        move |button| {
            if let Some(id) = &message_id {
                button.clipboard().set_text(id);
                overlay.add_toast(adw::Toast::new("Message-ID copied"));
            }
        }
    ));
    buttons.append(&copy_id);

    let raw = gtk::Button::builder()
        .icon_name("text-x-generic-symbolic")
        .tooltip_text("Raw")
        .css_classes(["flat"])
        .build();
    raw.connect_clicked(glib::clone!(
        #[weak]
        nav,
        move |_| nav.push(&build_raw_page())
    ));
    buttons.append(&raw);

    let reply = gtk::Button::builder()
        .icon_name("mail-reply-sender-symbolic")
        .tooltip_text("Reply")
        .css_classes(["flat"])
        .build();
    let mailto = build_reply_mailto(mail);
    reply.connect_clicked(move |button| launch_uri(button, &mailto));
    buttons.append(&reply);

    buttons
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

fn build_body_view(mail: &Mail, overlay: &adw::ToastOverlay) -> gtk::Box {
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

    let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wrapper.append(&hscroll);

    let group = gio::SimpleActionGroup::new();
    group.add_action(&quote);
    group.add_action(&quote_with_date);
    group.add_action(&copy);
    group.add_action(&select_all);
    wrapper.insert_action_group("mailview", Some(&group));

    setup_context_menu(&view, &wrapper);

    wrapper
}

// GTK only appends extra-menu items after the built-in ones, so to put the
// quote items first the context menu is replaced wholesale.
fn setup_context_menu(view: &gtk::TextView, wrapper: &gtk::Box) {
    let quote_section = gio::Menu::new();
    quote_section.append(Some("_Quote Selection"), Some("mailview.quote-selection"));
    quote_section.append(Some("Quote With _Date"), Some("mailview.quote-with-date"));

    let edit_section = gio::Menu::new();
    edit_section.append(Some("_Copy"), Some("mailview.copy"));
    edit_section.append(Some("Select _All"), Some("mailview.select-all"));

    let menu = gio::Menu::new();
    menu.append_section(None, &quote_section);
    menu.append_section(None, &edit_section);

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
