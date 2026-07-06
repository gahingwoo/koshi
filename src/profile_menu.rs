//! The header-bar profile button and the popover it opens: the user's name
//! and email, the effective git-send-email settings, and — when the user has
//! configured `[sendemail "<name>"]` identities — a radio selector to switch
//! between them.
//!
//! All stock Adwaita: a boxed [`adw::PreferencesGroup`] list, standard style
//! classes, no custom CSS. The popover rebuilds its contents each time it
//! opens so it always reflects the live git configuration.

use adw::prelude::*;
use gtk::glib;

use crate::profile::{self, Profile};

/// The account button for the header bar: an icon that opens the profile
/// popover.
pub fn build_profile_button() -> gtk::MenuButton {
    let popover = gtk::Popover::new();
    // Repopulate on every open so a config change (or an identity switch made
    // in a previous open) is always reflected.
    popover.connect_show(|popover| {
        popover.set_child(Some(&build_content(popover, &profile::load())));
    });

    let button = gtk::MenuButton::builder()
        .icon_name("avatar-default-symbolic")
        .tooltip_text("Profile")
        .build();
    button.set_popover(Some(&popover));
    button.add_css_class("flat");
    button
}

fn build_content(popover: &gtk::Popover, profile: &Profile) -> gtk::Widget {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .width_request(340)
        .build();

    if profile.is_empty() {
        container.append(&build_empty_state());
        container.append(&build_footer(popover));
        return wrap_in_scroller(&container);
    }

    container.append(&build_header(profile));
    if !profile.identities.is_empty() {
        container.append(&build_identities(popover, profile));
    }
    if let Some(sending) = build_sending_group(profile) {
        container.append(&sending);
    }
    if let Some(status) = build_status(profile) {
        container.append(&status);
    }
    container.append(&build_footer(popover));

    wrap_in_scroller(&container)
}

/// Name and email at the top. The name falls back to the active identity, then
/// to the email, so the block is never blank when anything is configured.
fn build_header(profile: &Profile) -> gtk::Widget {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();

    let name = profile
        .user_name
        .clone()
        .or_else(|| profile.active().map(|id| id.name.clone()))
        .or_else(|| profile.user_email.clone())
        .unwrap_or_else(|| "Profile".to_string());
    block.append(
        &gtk::Label::builder()
            .label(name)
            .use_markup(false)
            .css_classes(["title-4"])
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build(),
    );

    if let Some(email) = &profile.user_email {
        block.append(
            &gtk::Label::builder()
                .label(email)
                .use_markup(false)
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .xalign(0.0)
                .selectable(true)
                .wrap(true)
                .build(),
        );
    }

    block.upcast()
}

/// The identity selector: one radio row per `[sendemail "<name>"]`
/// subsection. Selecting a row writes `sendemail.identity` and rebuilds the
/// popover so the active badge and effective settings update.
fn build_identities(popover: &gtk::Popover, profile: &Profile) -> gtk::Widget {
    let group = adw::PreferencesGroup::builder()
        .title("Identities")
        .description("git send-email")
        .build();

    let mut group_leader: Option<gtk::CheckButton> = None;
    for identity in &profile.identities {
        let is_active = profile.active_identity.as_deref() == Some(identity.name.as_str());

        let check = gtk::CheckButton::new();
        match &group_leader {
            Some(leader) => check.set_group(Some(leader)),
            None => group_leader = Some(check.clone()),
        }
        check.set_active(is_active);

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&identity.name))
            .activatable(true)
            .build();
        if let Some(email) = &identity.email {
            row.set_subtitle(&glib::markup_escape_text(email));
        }
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));

        if is_active {
            row.add_suffix(
                &gtk::Label::builder()
                    .label("Active")
                    .css_classes(["dim-label", "caption"])
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }

        // Connect after set_active so the initial state does not trigger a
        // write. A radio switch toggles two buttons — only act on the one
        // turning on.
        let name = identity.name.clone();
        check.connect_toggled(glib::clone!(
            #[weak]
            popover,
            move |check| {
                if !check.is_active() {
                    return;
                }
                profile::set_active_identity(&name);
                // Rebuild from an idle: repopulating the popover destroys the
                // very CheckButton whose handler is running.
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    popover,
                    move || {
                        popover.set_child(Some(&build_content(&popover, &profile::load())));
                    }
                ));
            }
        ));

        group.add(&row);
    }

    group.upcast()
}

/// The effective send-email settings as read-only property rows. Titled to
/// name the active identity when one is in effect.
fn build_sending_group(profile: &Profile) -> Option<gtk::Widget> {
    let settings = profile.effective_sendemail();
    if settings.is_empty() {
        return None;
    }

    let group = adw::PreferencesGroup::builder().title("Sending").build();
    // Key the description off the *resolved* identity, not the raw pointer: a
    // dangling `sendemail.identity` takes no effect, so the effective values
    // below are top-level only. The name is escaped because a
    // PreferencesGroup description always renders Pango markup.
    match profile.active() {
        Some(identity) => group.set_description(Some(&format!(
            "Using identity \u{201c}{}\u{201d}",
            glib::markup_escape_text(&identity.name)
        ))),
        None => group.set_description(Some("git send-email")),
    }

    for setting in &settings {
        group.add(&property_row(&setting.key, &setting.value));
    }

    Some(group.upcast())
}

/// A key/value row: the git key as the title, the value dimmed and
/// right-aligned. The value is plain text (never markup — `from` values carry
/// `<addr>`) and selectable so it can be copied.
fn property_row(key: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(key))
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label(value)
            .use_markup(false)
            .css_classes(["dim-label"])
            .halign(gtk::Align::End)
            .hexpand(true)
            .xalign(1.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(28)
            .selectable(true)
            .build(),
    );
    row
}

/// A one-line "sends via …" indicator using the effective transport.
fn build_status(profile: &Profile) -> Option<gtk::Widget> {
    let transport = profile.effective_transport()?;

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Start)
        .build();
    let icon = gtk::Image::from_icon_name("emblem-ok-symbolic");
    icon.add_css_class("success");
    row.append(&icon);
    row.append(
        &gtk::Label::builder()
            .label(format!("Sends via {transport}"))
            .use_markup(false)
            .css_classes(["success", "caption"])
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build(),
    );

    Some(row.upcast())
}

fn build_empty_state() -> gtk::Widget {
    let block = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    block.append(
        &gtk::Label::builder()
            .label("No git profile")
            .css_classes(["title-4"])
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .build(),
    );
    block.append(
        &gtk::Label::builder()
            .label(
                "Koshi reads your name, email, and SMTP settings from git. \
                 Configure git send-email to see them here.",
            )
            .use_markup(false)
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build(),
    );
    block.upcast()
}

/// The full-width "Account Settings" button, which opens Preferences and
/// dismisses the popover.
fn build_footer(popover: &gtk::Popover) -> gtk::Widget {
    let button = gtk::Button::builder()
        .label("Account Settings")
        .action_name("app.preferences")
        .build();
    button.connect_clicked(glib::clone!(
        #[weak]
        popover,
        move |_| popover.popdown()
    ));
    button.upcast()
}

/// Keep the popover a sensible size: cap its height and let the content
/// scroll, while sizing its width to the content.
fn wrap_in_scroller(child: &impl IsA<gtk::Widget>) -> gtk::Widget {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(560)
        .child(child)
        .build()
        .upcast()
}
