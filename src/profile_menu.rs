//! The header-bar profile button and the light popover it opens.
//!
//! The popover is menu-weight — who you are, which send-email identity is
//! active, where mail goes, and two lead-in rows — following the GNOME split
//! between a primary menu and a settings surface. The dense git-send-email
//! configuration lives in an [`adw::PreferencesDialog`] reached from the
//! "Sending details" row, not crammed into the popover.
//!
//! Everything is stock Adwaita with built-in style classes; no custom CSS, no
//! avatars. The popover rebuilds from live `git config` on every open, so an
//! identity switch (which just writes `sendemail.identity` and closes) is
//! reflected the next time it opens — no in-place rebuild needed.

use adw::prelude::*;
use gtk::glib;

use crate::askpass;
use crate::profile::{self, Profile, Setting};

/// `[sendemail]` keys shown under "Server" in the details dialog.
const SERVER_KEYS: &[&str] = &[
    "smtpServer",
    "smtpServerPort",
    "smtpServerOption",
    "smtpEncryption",
    "smtpUser",
    "smtpDomain",
    "smtpSslCertPath",
    "sendmailCmd",
];

/// `[sendemail]` keys shown under "Addressing"; everything else is "Behavior".
const ADDRESSING_KEYS: &[&str] = &[
    "from",
    "envelopeSender",
    "to",
    "cc",
    "toCmd",
    "ccCmd",
    "suppressCc",
    "suppressFrom",
];

/// The account button for the header bar: an icon that opens the profile
/// popover.
pub fn build_profile_button() -> gtk::MenuButton {
    let popover = gtk::Popover::new();
    let button = gtk::MenuButton::builder()
        .icon_name("avatar-default-symbolic")
        .tooltip_text("Profile")
        .build();
    button.set_popover(Some(&popover));
    button.add_css_class("flat");

    // Repopulate on every open from live git config; pass the button so the
    // "Sending details" row can resolve the window to present its dialog on.
    popover.connect_show(glib::clone!(
        #[weak]
        button,
        move |popover| {
            popover.set_child(Some(&build_content(popover, &button, &profile::load())));
        }
    ));

    button
}

fn build_content(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(300)
        .build();

    if profile.is_empty() {
        root.append(&heading_label("No git profile"));
        root.append(&wrapped_dim_label(
            "Koshi reads your name, email, and SMTP settings from git. \
             Configure git send-email to see them here.",
        ));
        return root.upcast();
    }

    root.append(&build_header(profile));
    root.append(&build_identities(popover, button, profile));
    if let Some(transport) = build_transport(profile) {
        root.append(&transport);
    }
    if let Some(actions) = build_actions(popover, button, profile) {
        root.append(&actions);
    }

    root.upcast()
}

/// Name and email, plus — when exactly one identity is configured — a caption
/// naming it (there is nothing to switch to, so it is context, not a control).
fn build_header(profile: &Profile) -> gtk::Widget {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    let name = profile
        .user_name
        .clone()
        .or_else(|| profile.active().map(|id| id.name.clone()))
        .or_else(|| profile.user_email.clone())
        .unwrap_or_else(|| "Profile".to_string());
    block.append(&heading_label(&name));

    if let Some(email) = &profile.user_email {
        block.append(&build_email_pill(email));
    }

    if profile.identities.len() == 1 {
        let label = wrapped_dim_label(&format!("Identity: {}", profile.identities[0].name));
        label.add_css_class("caption");
        block.append(&label);
    }

    block.upcast()
}

/// The user's email as a monospace address pill, matching the To/Cc chips in
/// the thread view (built-in `address-chip` class). Clicking it copies the
/// whole address — it is not free-selectable text, so there is no stray
/// cursor or partial-selection.
fn build_email_pill(email: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .use_markup(true)
        .label(format!("<tt>{}</tt>", glib::markup_escape_text(email)))
        .css_classes(["caption"])
        .build();

    let pill = gtk::Button::builder()
        .child(&label)
        .halign(gtk::Align::Start)
        .tooltip_text("Copy email address")
        .css_classes(["address-chip"])
        .build();

    let email = email.to_string();
    pill.connect_clicked(move |pill| {
        pill.clipboard().set_text(&email);
    });

    pill.upcast()
}

/// The identity switcher and editor. One row per configured identity — with
/// an edit button that opens [`build_identity_editor_dialog`] pre-filled —
/// plus a trailing "Add Identity" row that opens the same dialog empty.
/// Shown whenever there is any git profile at all, even with zero identities
/// configured yet, since adding the first one is the point.
///
/// The row itself is only activatable (to switch identity) when there are two
/// or more: with zero or one there is nothing to switch to, and clicking the
/// lone row would be a confusing no-op. Activating a different row writes
/// `sendemail.identity` and closes the popover.
fn build_identities(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> gtk::Widget {
    let group = adw::PreferencesGroup::builder().title("Identities").build();
    let switchable = profile.identities.len() >= 2;

    for identity in &profile.identities {
        let is_active = profile.active_identity.as_deref() == Some(identity.name.as_str());

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&identity.name))
            .activatable(switchable)
            .build();
        if let Some(email) = &identity.email {
            row.set_subtitle(&glib::markup_escape_text(email));
        }
        if is_active && switchable {
            row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
        }

        let name = identity.name.clone();
        row.connect_activated(glib::clone!(
            #[weak]
            popover,
            move |_| {
                if !is_active {
                    profile::set_active_identity(&name);
                }
                popover.popdown();
            }
        ));

        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Edit identity")
            .css_classes(["flat"])
            .build();
        let identity = identity.clone();
        edit.connect_clicked(glib::clone!(
            #[weak]
            popover,
            #[weak]
            button,
            move |_| {
                let dialog = build_identity_editor_dialog(Some(&identity));
                dialog.present(button.root().and_downcast::<gtk::Window>().as_ref());
                popover.popdown();
            }
        ));
        row.add_suffix(&edit);

        group.add(&row);
    }

    let add_row = adw::ActionRow::builder()
        .title("Add Identity")
        .activatable(true)
        .build();
    add_row.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
    add_row.connect_activated(glib::clone!(
        #[weak]
        popover,
        #[weak]
        button,
        move |_| {
            let dialog = build_identity_editor_dialog(None);
            dialog.present(button.root().and_downcast::<gtk::Window>().as_ref());
            popover.popdown();
        }
    ));
    group.add(&add_row);

    group.upcast()
}

/// The create/edit form for one `[sendemail "<name>"]` identity. Save writes
/// straight to the user's global git config via
/// [`profile::set_identity_setting`] — this dialog is a friendlier way to
/// edit that file, not a parallel store, matching how the rest of Koshi's
/// identity handling works (see the module doc on [`crate::profile`]).
fn build_identity_editor_dialog(existing: Option<&profile::Identity>) -> adw::Dialog {
    // Built without a child first so the buttons below can hold a weak
    // reference to it and close it themselves on Save/Delete.
    let dialog = adw::Dialog::builder()
        .title(if existing.is_some() {
            "Edit Identity"
        } else {
            "Add Identity"
        })
        .content_width(440)
        .content_height(560)
        .build();

    let page = adw::PreferencesPage::new();
    page.set_description(
        "Identities are git send-email identities, kept in your global git \
         configuration \u{2014} saving here writes straight there; nothing is \
         stored by Koshi itself.",
    );

    let group = adw::PreferencesGroup::new();

    let name_row = adw::EntryRow::builder().title("Identity name").build();
    if let Some(identity) = existing {
        name_row.set_text(identity.name.as_str());
        // Renaming would mean moving every setting to a new git config
        // subsection, which this editor does not do - keep it fixed once
        // created, same as the git config file itself has no rename.
        name_row.set_sensitive(false);
    }
    group.add(&name_row);

    let field = |key: &str| -> String {
        existing
            .and_then(|identity| identity.settings.iter().find(|s| s.key == key))
            .map(|s| s.value.clone())
            .unwrap_or_default()
    };

    let from_row = adw::EntryRow::builder()
        .title("From (Name <email>)")
        .build();
    from_row.set_text(&field("from"));
    group.add(&from_row);

    let smtp_server_row = adw::EntryRow::builder().title("SMTP server").build();
    smtp_server_row.set_text(&field("smtpServer"));
    group.add(&smtp_server_row);

    let smtp_port_row = adw::EntryRow::builder().title("SMTP port").build();
    smtp_port_row.set_text(&field("smtpServerPort"));
    group.add(&smtp_port_row);

    let encryption_row = adw::ComboRow::builder()
        .title("Encryption")
        .model(&gtk::StringList::new(&["None", "SSL", "TLS"]))
        .build();
    encryption_row.set_selected(
        match field("smtpEncryption").to_ascii_lowercase().as_str() {
            "ssl" => 1,
            "tls" => 2,
            _ => 0,
        },
    );
    group.add(&encryption_row);

    let smtp_user_row = adw::EntryRow::builder().title("SMTP username").build();
    smtp_user_row.set_text(&field("smtpUser"));
    group.add(&smtp_user_row);

    let sendmail_row = adw::EntryRow::builder()
        .title("Local sendmail command (instead of SMTP)")
        .build();
    sendmail_row.set_text(&field("sendmailCmd"));
    group.add(&sendmail_row);

    page.add(&group);

    let overlay = adw::ToastOverlay::new();
    let header = adw::HeaderBar::new();
    // AdwDialog forces its own close button onto the header's start side with
    // no way to move it - on macOS, replace it with one of our own on the end
    // side, matching how the main window's buttons were moved there too. See
    // the comment on main.rs's move_dialog_close_button_to_end.
    #[cfg(target_os = "macos")]
    {
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        close.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                dialog.close();
            }
        ));
        header.pack_end(&close);
    }

    let save = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    header.pack_end(&save);
    let is_new = existing.is_none();
    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        overlay,
        #[weak]
        name_row,
        #[weak]
        from_row,
        #[weak]
        smtp_server_row,
        #[weak]
        smtp_port_row,
        #[weak]
        encryption_row,
        #[weak]
        smtp_user_row,
        #[weak]
        sendmail_row,
        move |_| {
            let name = name_row.text().trim().to_string();
            if name.is_empty() {
                overlay.add_toast(adw::Toast::new("Identity name is required"));
                return;
            }
            let encryption = match encryption_row.selected() {
                1 => "ssl",
                2 => "tls",
                _ => "",
            };
            let fields = [
                ("from", from_row.text()),
                ("smtpserver", smtp_server_row.text()),
                ("smtpserverport", smtp_port_row.text()),
                ("smtpencryption", encryption.into()),
                ("smtpuser", smtp_user_row.text()),
                ("sendmailcmd", sendmail_row.text()),
            ];
            let mut ok = true;
            for (key, value) in fields {
                let value = value.trim();
                let value = (!value.is_empty()).then_some(value);
                if !profile::set_identity_setting(&name, key, value) {
                    ok = false;
                }
            }
            if !ok {
                overlay.add_toast(adw::Toast::new("Failed to save identity"));
                return;
            }
            // A brand-new identity is the obvious thing to send as next;
            // editing an existing one leaves whichever is active alone.
            if is_new {
                profile::set_active_identity(&name);
            }
            dialog.close();
        }
    ));

    if let Some(identity) = existing {
        let delete = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Delete identity")
            .css_classes(["flat", "destructive-action"])
            .build();
        header.pack_start(&delete);
        let name = identity.name.clone();
        delete.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            overlay,
            move |_| {
                if profile::delete_identity(&name) {
                    dialog.close();
                } else {
                    overlay.add_toast(adw::Toast::new("Failed to delete identity"));
                }
            }
        ));
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    overlay.set_child(Some(&toolbar));
    dialog.set_child(Some(&overlay));

    dialog
}

/// A single calm row saying where mail goes: server as title, "Port 465 · SSL"
/// as subtitle (the Wi-Fi-row idiom). Absent when nothing sends mail.
fn build_transport(profile: &Profile) -> Option<gtk::Widget> {
    // Nothing routes mail (no SMTP server, no sendmail command) — no row.
    profile.effective_transport()?;

    let settings = profile.effective_sendemail();
    let get = |key: &str| {
        settings
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.value.as_str())
    };

    let (title, subtitle) = if let Some(server) = get("smtpServer") {
        let mut parts = Vec::new();
        if let Some(port) = get("smtpServerPort") {
            parts.push(format!("Port {port}"));
        }
        if let Some(encryption) = get("smtpEncryption") {
            parts.push(encryption_label(encryption));
        }
        (server.to_string(), parts.join(" \u{00b7} "))
    } else if let Some(command) = get("sendmailCmd") {
        ("Local sendmail".to_string(), command.to_string())
    } else {
        return None;
    };

    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&title))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
    if !subtitle.is_empty() {
        row.set_subtitle(&glib::markup_escape_text(&subtitle));
    }
    group.add(&row);

    Some(group.upcast())
}

fn encryption_label(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "ssl" => "SSL".to_string(),
        "tls" => "TLS".to_string(),
        "" | "none" => "No encryption".to_string(),
        _ => value.to_string(),
    }
}

/// The "Sending details" row, which opens the config dialog. Shown only when
/// there is git send-email config to show.
fn build_actions(
    popover: &gtk::Popover,
    button: &gtk::MenuButton,
    profile: &Profile,
) -> Option<gtk::Widget> {
    if profile.effective_sendemail().is_empty() {
        return None;
    }

    let group = adw::PreferencesGroup::new();

    let row = adw::ActionRow::builder()
        .title("Sending details")
        .subtitle("SMTP and git send-email options")
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

    let profile = profile.clone();
    row.connect_activated(glib::clone!(
        #[weak]
        popover,
        #[weak]
        button,
        move |_| {
            let dialog = build_sending_dialog(&profile);
            // Present on the top-level window (a stable widget) and only
            // then dismiss the popover. Presenting relative to the row
            // inside the closing popover serialises the dialog behind the
            // popover's dismissal grab/animation, which is the source of
            // the open lag; the window is always mapped, so it does not.
            dialog.present(button.root().and_downcast::<gtk::Window>().as_ref());
            popover.popdown();
        }
    ));
    group.add(&row);

    Some(group.upcast())
}

/// The full git-send-email configuration, split into Server / Addressing /
/// Behavior groups of read-only `property` rows with per-value copy buttons.
///
/// A plain `adw::Dialog` (not `adw::PreferencesDialog`): this is a read-only
/// view, so it needs none of the search / view-switcher machinery, and skipping
/// it makes the window open snappily.
fn build_sending_dialog(profile: &Profile) -> adw::Dialog {
    let page = adw::PreferencesPage::new();

    // One note above every group explaining where these values come from and
    // that they are set in git, not here.
    let description = match profile.active() {
        Some(identity) => format!(
            "Read from your git configuration, using the active identity \u{201c}{}\u{201d}. \
             To change them, edit your git send-email settings.",
            glib::markup_escape_text(&identity.name)
        ),
        None => "Read from your git configuration. To change them, edit your git \
                 send-email settings (git config)."
            .to_string(),
    };
    page.set_description(description.as_str());

    let settings = profile.effective_sendemail();
    let (mut server, mut addressing, mut behavior) = (Vec::new(), Vec::new(), Vec::new());
    for setting in &settings {
        if SERVER_KEYS.contains(&setting.key.as_str()) {
            server.push(setting);
        } else if ADDRESSING_KEYS.contains(&setting.key.as_str()) {
            addressing.push(setting);
        } else {
            behavior.push(setting);
        }
    }

    for (title, bucket) in [
        ("Server", server),
        ("Addressing", addressing),
        ("Behavior", behavior),
    ] {
        if bucket.is_empty() {
            continue;
        }
        let group = adw::PreferencesGroup::builder().title(title).build();
        for setting in bucket {
            group.add(&property_row(setting));
        }
        page.add(&group);
    }

    let overlay = adw::ToastOverlay::new();

    // When a password could be cached (SMTP transport with a user), offer to
    // forget it — the escape hatch for a password git approved but that did not
    // actually authenticate.
    if let Some((host, username)) = profile.smtp_credential() {
        page.add(&build_forget_password_group(host, username, &overlay));
    }

    let dialog = adw::Dialog::builder()
        .title("Send Email")
        .content_width(460)
        .content_height(620)
        .build();

    let header = adw::HeaderBar::new();
    // See the comment on main.rs's move_dialog_close_button_to_end: AdwDialog
    // forces its own close button onto the header's start side with no way to
    // move it, so on macOS this replaces it with one of our own on the end
    // side, matching the main window's buttons-on-the-right layout.
    #[cfg(target_os = "macos")]
    {
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        close.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                dialog.close();
            }
        ));
        header.pack_end(&close);
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    overlay.set_child(Some(&toolbar));
    dialog.set_child(Some(&overlay));

    dialog
}

/// A group with one destructive action: forget the SMTP password kept for this
/// session and evict it from git's credential helpers, so the next send prompts
/// for it again.
fn build_forget_password_group(
    host: String,
    username: String,
    overlay: &adw::ToastOverlay,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Password")
        .description(
            "Koshi never stores your SMTP password — it is kept only for this session, and \
             by git's own credential helper if you configured one.",
        )
        .build();

    let row = adw::ButtonRow::builder()
        .title("Forget SMTP Password")
        .build();
    row.add_css_class("destructive-action");
    row.connect_activated(glib::clone!(
        #[weak]
        overlay,
        move |_| {
            askpass::forget_all();
            profile::reject_smtp_credential(&host, &username);
            overlay.add_toast(adw::Toast::new("SMTP password forgotten"));
        }
    ));
    group.add(&row);

    group
}

/// A read-only key/value row: the git key as a de-emphasized title, the value
/// emphasized, with a flat button to copy it. The value is deliberately NOT
/// selectable — these are display-only, so there is no text cursor and nothing
/// looks editable; copying goes through the button.
fn property_row(setting: &Setting) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&setting.key))
        .css_classes(["property"])
        .build();
    // Plain text — a `from` value carries `<addr>` that must not be markup.
    row.set_subtitle(&glib::markup_escape_text(&setting.value));

    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Copy value")
        .css_classes(["flat"])
        .build();
    let value = setting.value.clone();
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&value);
    });
    row.add_suffix(&copy);

    row
}

fn heading_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build()
}

fn wrapped_dim_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .css_classes(["dim-label"])
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build()
}
