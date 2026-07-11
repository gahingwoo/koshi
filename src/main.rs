mod composer;
mod favorites;
mod favorites_page;
mod highlight;
mod inbox_page;
mod list_page;
mod lore;
mod profile;
mod profile_menu;
mod remote_page;
mod thread_list_page;
mod thread_page;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use favorites_page::{FAVORITES_PAGE_NAME, build_favorites_page};
use inbox_page::{INBOX_LIST_TITLE, build_inbox_page};
use profile_menu::build_profile_button;
use thread_list_page::{build_search_page, build_thread_list_page};
use thread_page::{THREAD_PAGE_NAME, build_thread_page, toggle_overview};

const APP_ID: &str = "moe.nikableh.Koshi";

// Debug builds show the Devel icon so they are distinguishable from an
// installed release build.
const APP_ICON: &str = std::cfg_select! {
    debug_assertions => { "moe.nikableh.Koshi.Devel" }
    _ => { APP_ID }
};

fn main() -> glib::ExitCode {
    gio::resources_register_include!("koshi.gresource")
        .expect("failed to register resources");

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        favorites::init(glib::user_data_dir().join("koshi").join("favorites.json"));
        load_css();
        register_bundled_icons();
        // Use the bundled app icon for window/taskbar decorations. When Koshi
        // is installed its desktop file points the shell at the same icon; this
        // covers the uninstalled `cargo run` case and titlebar fallbacks.
        gtk::Window::set_default_icon_name(APP_ICON);
    });
    app.connect_activate(build_ui);
    app.run()
}

// The single user-approved custom-CSS exception: compact address chips.
// Everything else must stay stock Adwaita.
fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "button.address-chip { min-height: 0; padding: 5px 8px; border-radius: 9999px; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

// Icons bundled in the gresource (e.g. the mirrored rewrap arrow) are not in
// the system theme, so the resource icon dir must be on the theme's path.
fn register_bundled_icons() {
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_resource_path("/moe/nikableh/Koshi/icons");
    }
}

fn build_ui(app: &adw::Application) {
    let (window, tab_view) = build_window(app);
    open_new_tab(&tab_view);
    window.present();
}

fn build_window(app: &adw::Application) -> (adw::ApplicationWindow, adw::TabView) {
    let search_entry = build_search_entry();

    let tab_view = adw::TabView::new();
    let tab_bar = adw::TabBar::builder().view(&tab_view).build();
    setup_tab_context_menu(&tab_view);

    let go_back = gio::SimpleAction::new("go-back", None);
    go_back.set_enabled(false);

    let thread_overview = gio::SimpleAction::new("toggle-thread-overview", None);
    thread_overview.set_enabled(false);
    thread_overview.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        move |_, _| {
            if let Some(page) = selected_nav(&tab_view).and_then(|nav| nav.visible_page()) {
                toggle_overview(&page);
            }
        }
    ));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&build_header_bar(
        &search_entry,
        &tab_view,
        &go_back,
        &thread_overview,
    ));
    toolbar_view.add_top_bar(&tab_bar);
    toolbar_view.set_content(Some(&tab_view));

    go_back.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        move |_, _| {
            if let Some(nav) = selected_nav(&tab_view) {
                nav.pop();
            }
        }
    ));

    tab_view.connect_selected_page_notify(glib::clone!(
        #[strong]
        go_back,
        #[strong]
        thread_overview,
        move |view| {
            let nav = selected_nav(view);
            go_back.set_enabled(nav.as_ref().is_some_and(nav_can_pop));
            thread_overview.set_enabled(nav.as_ref().is_some_and(nav_shows_thread));
        }
    ));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Koshi")
        .default_width(1000)
        .default_height(625)
        .content(&toolbar_view)
        .build();

    window.add_action(&go_back);
    window.add_action(&thread_overview);
    setup_actions(app, &window, &search_entry);
    setup_search(&search_entry, &tab_view);

    (window, tab_view)
}

/// Dispatch the search entry: a lore.kernel.org URL or a Message-ID opens
/// that thread directly (the `r` pseudo-list resolves an id across every
/// list); anything else is a lore query over the `all` pseudo-list.
fn setup_search(search_entry: &gtk::SearchEntry, tab_view: &adw::TabView) {
    search_entry.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        move |entry| {
            // Collapse all whitespace (pasted text can carry newlines and tabs)
            // into single spaces so the query stays one line — both for the
            // search request and for the header/status labels that display it,
            // where embedded newlines would defeat their ellipsization.
            let text = entry.text().split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                return;
            }
            let Some(nav) = selected_nav(&tab_view) else {
                return;
            };
            match parse_lore_url(&text) {
                Some((list, Some(message_id))) => {
                    nav.push(&build_thread_page(&nav, &list, &message_id));
                }
                Some((list, None)) => {
                    nav.push(&build_thread_list_page(&nav, &list, ""));
                }
                None if looks_like_message_id(&text) => {
                    let message_id = text.trim_matches(['<', '>']);
                    nav.push(&build_thread_page(&nav, "r", message_id));
                }
                None => nav.push(&build_search_page(&nav, &text)),
            }
            entry.set_text("");
        }
    ));
}

/// Split a lore.kernel.org URL into its list and, when it points at a
/// message, the Message-ID.
fn parse_lore_url(text: &str) -> Option<(String, Option<String>)> {
    let rest = text.split("lore.kernel.org/").nth(1)?;
    let mut segments = rest
        .split(['/', '?', '#'])
        .filter(|segment| !segment.is_empty());
    let list = segments.next()?;
    let message_id = segments.next().filter(|segment| segment.contains('@'));
    Some((list.to_string(), message_id.map(str::to_string)))
}

/// `<id@host>`, or a bare address-shaped token that can't be a lore query.
fn looks_like_message_id(text: &str) -> bool {
    if text.starts_with('<') && text.ends_with('>') && text.contains('@') {
        return true;
    }
    !text.contains(char::is_whitespace)
        && !text.contains(':')
        && text.contains('@')
        && text.contains('.')
}

fn setup_tab_context_menu(tab_view: &adw::TabView) {
    let menu = gio::Menu::new();
    menu.append(Some("Move to New _Window"), Some("tab.move-to-new-window"));

    let pin = gio::MenuItem::new(Some("_Pin Tab"), Some("tab.pin"));
    pin.set_attribute_value("hidden-when", Some(&"action-disabled".into()));
    menu.append_item(&pin);

    let unpin = gio::MenuItem::new(Some("Un_pin Tab"), Some("tab.unpin"));
    unpin.set_attribute_value("hidden-when", Some(&"action-disabled".into()));
    menu.append_item(&unpin);

    menu.append(Some("Close _Other Tabs"), Some("tab.close-others"));
    menu.append(Some("Close _All Tabs"), Some("tab.close-all"));
    menu.append(Some("_Close"), Some("tab.close"));

    tab_view.set_menu_model(Some(&menu));

    let target: Rc<RefCell<Option<adw::TabPage>>> = Rc::new(RefCell::new(None));
    let group = gio::SimpleActionGroup::new();

    let move_to_new_window = gio::SimpleAction::new("move-to-new-window", None);
    move_to_new_window.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        #[strong]
        target,
        move |_, _| {
            let Some(page) = target.borrow().clone() else {
                return;
            };
            let Some(window) = tab_view.root().and_downcast::<gtk::Window>() else {
                return;
            };
            let Some(app) = window.application().and_downcast::<adw::Application>() else {
                return;
            };
            let (new_window, new_view) = build_window(&app);
            tab_view.transfer_page(&page, &new_view, 0);
            new_window.present();
        }
    ));

    let pin_action = gio::SimpleAction::new("pin", None);
    pin_action.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        #[strong]
        target,
        move |_, _| {
            if let Some(page) = target.borrow().as_ref() {
                tab_view.set_page_pinned(page, true);
            }
        }
    ));

    let unpin_action = gio::SimpleAction::new("unpin", None);
    unpin_action.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        #[strong]
        target,
        move |_, _| {
            if let Some(page) = target.borrow().as_ref() {
                tab_view.set_page_pinned(page, false);
            }
        }
    ));

    let close_others = gio::SimpleAction::new("close-others", None);
    close_others.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        #[strong]
        target,
        move |_, _| {
            if let Some(page) = target.borrow().as_ref() {
                tab_view.close_other_pages(page);
            }
        }
    ));

    let close_all = gio::SimpleAction::new("close-all", None);
    close_all.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        move |_, _| {
            let pages: Vec<adw::TabPage> =
                (0..tab_view.n_pages()).map(|i| tab_view.nth_page(i)).collect();
            for page in pages {
                if page.is_pinned() {
                    tab_view.set_page_pinned(&page, false);
                }
                tab_view.close_page(&page);
            }
        }
    ));

    let close = gio::SimpleAction::new("close", None);
    close.connect_activate(glib::clone!(
        #[weak]
        tab_view,
        #[strong]
        target,
        move |_, _| {
            if let Some(page) = target.borrow().as_ref() {
                tab_view.close_page(page);
            }
        }
    ));

    tab_view.connect_setup_menu(glib::clone!(
        #[strong]
        target,
        #[strong]
        move_to_new_window,
        #[strong]
        pin_action,
        #[strong]
        unpin_action,
        #[strong]
        close_others,
        #[strong]
        close,
        move |view, page| {
            *target.borrow_mut() = page.cloned();
            let pinned = page.is_some_and(|p| p.is_pinned());
            move_to_new_window.set_enabled(page.is_some() && view.n_pages() > 1);
            pin_action.set_enabled(page.is_some() && !pinned);
            unpin_action.set_enabled(pinned);
            close_others.set_enabled(page.is_some() && view.n_pages() > 1);
            close.set_enabled(page.is_some() && !pinned);
        }
    ));

    group.add_action(&move_to_new_window);
    group.add_action(&pin_action);
    group.add_action(&unpin_action);
    group.add_action(&close_others);
    group.add_action(&close_all);
    group.add_action(&close);
    tab_view.insert_action_group("tab", Some(&group));
}

fn open_new_tab(tab_view: &adw::TabView) {
    let nav = adw::NavigationView::new();
    nav.push(&build_inbox_page(&nav));

    let tab_page = tab_view.append(&nav);
    tab_page.set_title(INBOX_LIST_TITLE);
    tab_view.set_selected_page(&tab_page);

    // The tab title tracks the visible page's title property, not just its
    // value at navigation time: thread pages start as "Loading…" and retitle
    // themselves once fetched.
    let title_binding: Rc<RefCell<Option<glib::Binding>>> = Rc::new(RefCell::new(None));
    nav.connect_visible_page_notify(glib::clone!(
        #[weak]
        tab_page,
        move |nav| {
            if let Some(binding) = title_binding.take() {
                binding.unbind();
            }
            if let Some(page) = nav.visible_page() {
                let binding = page
                    .bind_property("title", &tab_page, "title")
                    .sync_create()
                    .build();
                title_binding.replace(Some(binding));
            }
            update_go_back_action(nav);
        }
    ));
}

fn selected_nav(tab_view: &adw::TabView) -> Option<adw::NavigationView> {
    tab_view
        .selected_page()
        .map(|page| page.child())
        .and_downcast::<adw::NavigationView>()
}

fn nav_can_pop(nav: &adw::NavigationView) -> bool {
    nav.visible_page()
        .and_then(|page| nav.previous_page(&page))
        .is_some()
}

fn nav_shows_thread(nav: &adw::NavigationView) -> bool {
    nav.visible_page()
        .is_some_and(|page| page.widget_name() == THREAD_PAGE_NAME)
}

fn update_go_back_action(nav: &adw::NavigationView) {
    let Some(window) = nav.root().and_downcast::<adw::ApplicationWindow>() else {
        return;
    };
    if let Some(action) = window
        .lookup_action("go-back")
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(nav_can_pop(nav));
    }
    if let Some(action) = window
        .lookup_action("toggle-thread-overview")
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(nav_shows_thread(nav));
    }
}

fn build_search_entry() -> gtk::SearchEntry {
    gtk::SearchEntry::builder()
        .placeholder_text("Search lore, a Message-ID, or a lore.kernel.org URL")
        .hexpand(true)
        .build()
}

fn build_header_bar(
    search_entry: &gtk::SearchEntry,
    tab_view: &adw::TabView,
    go_back: &gio::SimpleAction,
    thread_overview: &gio::SimpleAction,
) -> adw::HeaderBar {
    let header = adw::HeaderBar::new();

    let back_button = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back")
        .action_name("win.go-back")
        .visible(false)
        .build();
    go_back
        .bind_property("enabled", &back_button, "visible")
        .sync_create()
        .build();
    header.pack_start(&back_button);

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

    let favorites_button = gtk::Button::builder()
        .icon_name("starred-symbolic")
        .tooltip_text("Favorites")
        .build();
    favorites_button.connect_clicked(glib::clone!(
        #[weak]
        tab_view,
        move |_| {
            let Some(nav) = selected_nav(&tab_view) else {
                return;
            };
            let already_there = nav
                .visible_page()
                .is_some_and(|page| page.widget_name() == FAVORITES_PAGE_NAME);
            if !already_there {
                nav.push(&build_favorites_page(&nav));
            }
        }
    ));

    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .tightening_threshold(400)
        .hexpand(true)
        .child(search_entry)
        .build();
    header.set_title_widget(Some(&clamp));

    // Like the back button, only shown where it applies: on a thread page.
    let overview_button = gtk::Button::builder()
        .icon_name("sidebar-show-right-symbolic")
        .tooltip_text("Thread Overview")
        .action_name("win.toggle-thread-overview")
        .visible(false)
        .build();
    thread_overview
        .bind_property("enabled", &overview_button, "visible")
        .sync_create()
        .build();

    header.pack_end(&build_primary_menu_button());
    header.pack_end(&build_profile_button());
    header.pack_end(&favorites_button);
    header.pack_end(&overview_button);

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

    app.add_action_entries([preferences, shortcuts, about]);

    app.set_accels_for_action("win.focus-search", &["<Control>l"]);
    app.set_accels_for_action("win.toggle-thread-overview", &["F9"]);
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
        "Thread overview",
        "win.toggle-thread-overview",
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
        .application_icon(APP_ICON)
        .developer_name("Nika Krasnova")
        .version(env!("CARGO_PKG_VERSION"))
        .comments(env!("CARGO_PKG_DESCRIPTION"))
        .website(env!("CARGO_PKG_HOMEPAGE"))
        .issue_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"))
        .developers(["Nika Krasnova <nika@nikableh.moe>"])
        .copyright("© 2026 Nika Krasnova")
        .license_type(gtk::License::Gpl30)
        .build();
    about.present(app.active_window().as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lore_message_urls_yield_list_and_message_id() {
        assert_eq!(
            parse_lore_url("https://lore.kernel.org/lkml/20260705200723.66564929@pumpkin/"),
            Some((
                "lkml".to_string(),
                Some("20260705200723.66564929@pumpkin".to_string())
            ))
        );
        assert_eq!(
            parse_lore_url("lore.kernel.org/r/some-id@example.org/T/#u"),
            Some(("r".to_string(), Some("some-id@example.org".to_string())))
        );
    }

    #[test]
    fn lore_list_urls_yield_only_the_list() {
        assert_eq!(
            parse_lore_url("https://lore.kernel.org/bpf/"),
            Some(("bpf".to_string(), None))
        );
        assert_eq!(parse_lore_url("[PATCH] not a url"), None);
    }

    #[test]
    fn message_ids_are_recognized_but_queries_are_not() {
        assert!(looks_like_message_id("<some-id@example.org>"));
        assert!(looks_like_message_id("20260705200723.66564929@pumpkin.example"));
        assert!(!looks_like_message_id("f:torvalds@linux-foundation.org"));
        assert!(!looks_like_message_id("sched fix regression"));
    }

    #[test]
    fn bundled_rewrap_icon_is_in_the_gresource() {
        gtk::gio::resources_register_include!("koshi.gresource").unwrap();
        let data = gtk::gio::resources_lookup_data(
            "/moe/nikableh/Koshi/icons/scalable/actions/koshi-rewrap-symbolic.svg",
            gtk::gio::ResourceLookupFlags::NONE,
        )
        .unwrap();
        let svg = String::from_utf8(data.to_vec()).unwrap();
        // The bundled asset must stay the x-axis-mirrored variant.
        assert!(svg.contains("matrix(1 0 0 -1 0 16)"), "flip lost: {svg}");
    }
}
