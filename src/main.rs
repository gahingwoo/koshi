mod inbox_page;
mod thread_list_page;

use adw::prelude::*;
use gtk::{gio, glib};

use inbox_page::{INBOX_LIST_TITLE, build_inbox_page};

const APP_ID: &str = "moe.nikableh.Koshi";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let search_entry = build_search_entry();

    let tab_view = adw::TabView::new();
    let tab_bar = adw::TabBar::builder()
        .view(&tab_view)
        .autohide(false)
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&build_header_bar(&search_entry, &tab_view));
    toolbar_view.add_top_bar(&tab_bar);
    toolbar_view.set_content(Some(&tab_view));

    open_new_tab(&tab_view);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Koshi")
        .default_width(1000)
        .default_height(625)
        .content(&toolbar_view)
        .build();

    setup_actions(app, &window, &search_entry);

    window.present();
}

fn open_new_tab(tab_view: &adw::TabView) {
    let nav = adw::NavigationView::new();
    nav.push(&build_inbox_page(&nav));

    let tab_page = tab_view.append(&nav);
    tab_page.set_title(INBOX_LIST_TITLE);
    tab_view.set_selected_page(&tab_page);

    nav.connect_visible_page_notify(glib::clone!(
        #[weak]
        tab_page,
        move |nav| {
            if let Some(page) = nav.visible_page() {
                tab_page.set_title(&page.title());
            }
        }
    ));
}

fn build_search_entry() -> gtk::SearchEntry {
    gtk::SearchEntry::builder()
        .placeholder_text("Search lore, a Message-ID, or a lore.kernel.org URL")
        .hexpand(true)
        .build()
}

fn build_search_overlay(search_entry: &gtk::SearchEntry) -> gtk::Overlay {
    let badge = adw::ShortcutLabel::new("<Control>l");
    badge.set_halign(gtk::Align::End);
    badge.set_valign(gtk::Align::Center);
    badge.set_margin_end(8);
    badge.set_can_target(false);
    badge.add_css_class("dim-label");
    badge.add_css_class("caption");

    let update_badge = glib::clone!(
        #[weak]
        search_entry,
        #[weak]
        badge,
        move |focused: bool| {
            badge.set_visible(!focused && search_entry.text().is_empty());
        }
    );

    search_entry.connect_changed(glib::clone!(
        #[strong]
        update_badge,
        move |entry| {
            update_badge(entry.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN));
        }
    ));

    let focus_controller = gtk::EventControllerFocus::new();
    focus_controller.connect_enter(glib::clone!(
        #[strong]
        update_badge,
        move |_| update_badge(true)
    ));
    focus_controller.connect_leave(move |_| update_badge(false));
    search_entry.add_controller(focus_controller);

    let overlay = gtk::Overlay::builder().child(search_entry).build();
    overlay.add_overlay(&badge);
    overlay
}

fn build_header_bar(search_entry: &gtk::SearchEntry, tab_view: &adw::TabView) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();

    let new_tab_button = gtk::Button::builder()
        .icon_name("tab-new-symbolic")
        .tooltip_text("New Tab")
        .build();
    new_tab_button.connect_clicked(glib::clone!(
        #[weak]
        tab_view,
        move |_| open_new_tab(&tab_view)
    ));
    header.pack_start(&new_tab_button);

    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .tightening_threshold(400)
        .hexpand(true)
        .child(&build_search_overlay(search_entry))
        .build();
    header.set_title_widget(Some(&clamp));

    header.pack_end(&build_primary_menu_button());
    header.pack_end(&build_account_button());

    header
}

fn build_primary_menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("_Preferences"), Some("app.preferences"));
    menu.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
    menu.append(Some("_About Koshi"), Some("app.about"));

    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build()
}

fn build_account_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    menu.append(Some("Sign In"), Some("app.sign-in"));
    menu.append(Some("Manage Accounts"), Some("app.manage-accounts"));

    let button = gtk::MenuButton::builder()
        .icon_name("avatar-default-symbolic")
        .menu_model(&menu)
        .tooltip_text("Account")
        .build();
    button.add_css_class("flat");
    button
}

fn setup_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    search_entry: &gtk::SearchEntry,
) {
    let focus_search = gio::ActionEntry::builder("focus-search")
        .activate(glib::clone!(
            #[weak]
            search_entry,
            move |_win: &adw::ApplicationWindow, _, _| {
                search_entry.grab_focus();
            }
        ))
        .build();
    window.add_action_entries([focus_search]);

    let preferences = gio::ActionEntry::builder("preferences")
        .activate(|app: &adw::Application, _, _| {
            show_preferences(app);
        })
        .build();

    let shortcuts = gio::ActionEntry::builder("shortcuts")
        .activate(|app: &adw::Application, _, _| {
            show_shortcuts(app);
        })
        .build();

    let about = gio::ActionEntry::builder("about")
        .activate(|app: &adw::Application, _, _| {
            show_about(app);
        })
        .build();

    // Stubs for the account menu so its items are not rendered insensitive.
    let sign_in = gio::ActionEntry::builder("sign-in")
        .activate(|_: &adw::Application, _, _| {})
        .build();
    let manage_accounts = gio::ActionEntry::builder("manage-accounts")
        .activate(|_: &adw::Application, _, _| {})
        .build();

    app.add_action_entries([preferences, shortcuts, about, sign_in, manage_accounts]);

    app.set_accels_for_action("win.focus-search", &["<Control>l"]);
    app.set_accels_for_action("app.preferences", &["<Control>comma"]);
    app.set_accels_for_action("app.shortcuts", &["<Control>question"]);
}

fn show_preferences(app: &adw::Application) {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("emblem-system-symbolic")
        .build();
    page.add(&adw::PreferencesGroup::builder().title("General").build());

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(app.active_window().as_ref());
}

fn show_shortcuts(app: &adw::Application) {
    let section = adw::ShortcutsSection::new(Some("General"));
    section.add(adw::ShortcutsItem::from_action(
        "Focus search",
        "win.focus-search",
    ));
    section.add(adw::ShortcutsItem::from_action(
        "Preferences",
        "app.preferences",
    ));
    section.add(adw::ShortcutsItem::from_action(
        "Keyboard shortcuts",
        "app.shortcuts",
    ));

    let dialog = adw::ShortcutsDialog::new();
    dialog.add(section);
    dialog.present(app.active_window().as_ref());
}

fn show_about(app: &adw::Application) {
    let about = adw::AboutDialog::builder()
        .application_name("Koshi")
        .application_icon(APP_ID)
        .developer_name("Nika")
        .version(env!("CARGO_PKG_VERSION"))
        .build();
    about.present(app.active_window().as_ref());
}
