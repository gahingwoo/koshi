use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::list_page::build_list_page;
use crate::lore::{self, Sort, ThreadSummary};
use crate::remote_page::RemoteContent;
use crate::thread_page::{build_thread_page, launch_uri};

#[derive(Clone)]
enum Mode {
    /// Recent thread roots of one list.
    Recent { list: String },
    /// Full-text search results (over the `all` pseudo-list).
    Search { list: String, query: String },
}

impl Mode {
    fn list(&self) -> &str {
        match self {
            Mode::Recent { list } | Mode::Search { list, .. } => list,
        }
    }

    async fn fetch(
        &self,
        offset: usize,
        sort: Sort,
        cancellable: &gio::Cancellable,
    ) -> Result<Vec<ThreadSummary>, lore::Error> {
        match self {
            // Browsing a list has no search terms to rank, so it is always by
            // date; `sort` only applies to full-text search.
            Mode::Recent { list } => lore::fetch_thread_roots(list, offset, cancellable).await,
            Mode::Search { list, query } => {
                lore::search(list, query, offset, sort, cancellable).await
            }
        }
    }
}

pub fn build_thread_list_page(
    nav: &adw::NavigationView,
    inbox_name: &str,
    inbox_description: &str,
) -> adw::NavigationPage {
    build_page(
        nav,
        Mode::Recent { list: inbox_name.to_string() },
        inbox_name,
        inbox_name,
        inbox_description,
    )
}

pub fn build_search_page(nav: &adw::NavigationView, query: &str) -> adw::NavigationPage {
    build_page(
        nav,
        Mode::Search { list: "all".to_string(), query: query.to_string() },
        &format!("Search: {query}"),
        "Search results",
        &format!("Matches for “{query}” across all of lore.kernel.org"),
    )
}

fn build_page(
    nav: &adw::NavigationView,
    mode: Mode,
    page_title: &str,
    heading: &str,
    description: &str,
) -> adw::NavigationPage {
    let remote = RemoteContent::new();
    let sort = Rc::new(Cell::new(Sort::default()));

    let refresh_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh")
        .css_classes(["flat"])
        .build();
    refresh_button.connect_clicked(glib::clone!(
        #[strong]
        remote,
        #[weak]
        nav,
        #[strong]
        mode,
        #[strong]
        sort,
        move |_| load(remote.clone(), nav, mode.clone(), sort.get())
    ));

    // Relevance ranking only makes sense for a full-text search; a plain list
    // browse is always newest-first, so the sort control is search-only.
    let mut actions = vec![refresh_button.upcast::<gtk::Widget>()];
    if matches!(mode, Mode::Search { .. }) {
        let sort_button = build_sort_button(glib::clone!(
            #[strong]
            remote,
            #[weak]
            nav,
            #[strong]
            mode,
            #[strong]
            sort,
            move |chosen| {
                sort.set(chosen);
                load(remote.clone(), nav, mode.clone(), chosen);
            }
        ));
        actions.push(sort_button.upcast());
    }

    let page = build_list_page(page_title, heading, description, &actions, remote.widget());

    load(remote, nav.clone(), mode, sort.get());
    page
}

fn load(remote: RemoteContent, nav: adw::NavigationView, mode: Mode, sort: Sort) {
    remote.show_loading();
    let cancellable = remote.cancellable();
    glib::spawn_future_local(async move {
        match mode.fetch(0, sort, &cancellable).await {
            Ok(threads) if threads.is_empty() => {
                let status = adw::StatusPage::builder()
                    .icon_name("system-search-symbolic")
                    .title("No Results")
                    .build();
                remote.show_content(&status);
            }
            Ok(threads) => {
                remote.show_content(&build_thread_list(&nav, &cancellable, &mode, sort, threads));
            }
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                let weak = remote.downgrade();
                let nav = nav.downgrade();
                remote.show_error(
                    &error,
                    move || {
                        if let (Some(remote), Some(nav)) = (weak.upgrade(), nav.upgrade()) {
                            load(remote, nav, mode.clone(), sort);
                        }
                    },
                    None,
                );
            }
        }
    });
}

fn build_sort_button(on_change: impl Fn(Sort) + 'static) -> gtk::MenuButton {
    let sort_action =
        gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &"date".into());
    sort_action.connect_activate(move |action, param| {
        if let Some(param) = param {
            // Selecting the already-active option is a no-op; only re-fetch on
            // a real change.
            if action.state().as_ref() == Some(param) {
                return;
            }
            action.set_state(param);
            let sort = match param.str() {
                Some("relevance") => Sort::Relevance,
                _ => Sort::Date,
            };
            on_change(sort);
        }
    });
    let group = gio::SimpleActionGroup::new();
    group.add_action(&sort_action);

    let menu = gio::Menu::new();
    menu.append(Some("Date"), Some("threads.sort::date"));
    menu.append(Some("Relevance"), Some("threads.sort::relevance"));

    let button = gtk::MenuButton::builder()
        .icon_name("view-sort-descending-symbolic")
        .menu_model(&menu)
        .tooltip_text("Sort")
        .halign(gtk::Align::End)
        .css_classes(["flat"])
        .build();
    button.insert_action_group("threads", Some(&group));
    button
}

fn build_thread_list(
    nav: &adw::NavigationView,
    cancellable: &gio::Cancellable,
    mode: &Mode,
    sort: Sort,
    threads: Vec<ThreadSummary>,
) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        // The list sits inside a vexpanding stack; without this it stretches
        // and the boxed-list shadow outlines the empty space below the rows.
        .valign(gtk::Align::Start)
        .css_classes(["boxed-list"])
        .build();

    let full_page = threads.len() >= lore::PAGE_SIZE;
    for thread in &threads {
        list.append(&build_thread_row(thread, mode.list()));
    }
    if full_page {
        list.append(&build_load_more_row());
    }

    let threads = Rc::new(RefCell::new(threads));
    // The cancellable is captured instead of the whole RemoteContent: this
    // closure lives inside the stack, and a strong stack reference here would
    // be a leaky cycle.
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        #[strong]
        threads,
        #[strong]
        cancellable,
        #[strong]
        mode,
        move |list, row| {
            let index = row.index() as usize;
            if index < threads.borrow().len() {
                let message_id = threads.borrow()[index].message_id.clone();
                nav.push(&build_thread_page(&nav, mode.list(), &message_id));
            } else {
                load_more(list, row, threads.clone(), mode.clone(), sort, cancellable.clone());
            }
        }
    ));

    list
}

/// Fetch the next page and splice it in where the Load More row sits.
fn load_more(
    list: &gtk::ListBox,
    row: &gtk::ListBoxRow,
    threads: Rc<RefCell<Vec<ThreadSummary>>>,
    mode: Mode,
    sort: Sort,
    cancellable: gio::Cancellable,
) {
    if !row.is_sensitive() {
        return; // already loading
    }
    row.set_sensitive(false);
    let list = list.clone();
    let row = row.clone();
    glib::spawn_future_local(async move {
        let offset = threads.borrow().len();
        match mode.fetch(offset, sort, &cancellable).await {
            Ok(more) => {
                list.remove(&row);
                let full_page = more.len() >= lore::PAGE_SIZE;
                for thread in &more {
                    list.append(&build_thread_row(thread, mode.list()));
                }
                threads.borrow_mut().extend(more);
                if full_page {
                    list.append(&build_load_more_row());
                }
            }
            Err(error) if error.is_cancelled() => {}
            // Make the row clickable again; activating it retries.
            Err(_) => row.set_sensitive(true),
        }
    });
}

fn build_load_more_row() -> adw::ButtonRow {
    adw::ButtonRow::builder().title("Load More").build()
}

fn build_thread_row(thread: &ThreadSummary, list: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(format!("<tt>{}</tt>", glib::markup_escape_text(&thread.subject)))
        .title_lines(1)
        .tooltip_text(&thread.subject)
        .subtitle(glib::markup_escape_text(&thread.author))
        .subtitle_lines(1)
        .activatable(true)
        .build();

    let timestamp = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    timestamp.append(
        &gtk::Label::builder()
            .label(format_date(&thread.updated))
            .halign(gtk::Align::End)
            .css_classes(["numeric", "caption"])
            .build(),
    );
    timestamp.append(
        &gtk::Label::builder()
            .label(thread.updated.format("%H:%M").unwrap_or_default())
            .halign(gtk::Align::End)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );
    row.add_suffix(&timestamp);

    add_row_actions(&row, list, &thread.message_id);

    row
}

/// Wire a thread row's secondary-click context menu ("Open in New Tab", "Open
/// on Web") and its middle-click shortcut for opening the thread in a
/// background tab.
fn add_row_actions(row: &adw::ActionRow, list: &str, message_id: &str) {
    // `message_id` arrives already stripped of angle brackets, but trim to be
    // safe and match the /r/ redirect URL used in the message reading view.
    let bare = message_id.trim().trim_start_matches('<').trim_end_matches('>');
    let url = format!("https://lore.kernel.org/r/{bare}/");
    let list = list.to_string();
    let message_id = message_id.to_string();

    let group = gio::SimpleActionGroup::new();

    let open_new_tab = gio::SimpleAction::new("open-new-tab", None);
    open_new_tab.connect_activate(glib::clone!(
        #[weak]
        row,
        #[strong]
        list,
        #[strong]
        message_id,
        move |_, _| open_in_new_tab(&row, &list, &message_id)
    ));
    group.add_action(&open_new_tab);

    let open_web = gio::SimpleAction::new("open-web", None);
    open_web.connect_activate(glib::clone!(
        #[weak]
        row,
        #[strong]
        url,
        move |_, _| launch_uri(&row, &url)
    ));
    group.add_action(&open_web);

    let menu = gio::Menu::new();
    menu.append(Some("Open in New _Tab"), Some("menu.open-new-tab"));
    menu.append(Some("Open on _Web"), Some("menu.open-web"));

    // The popover is built fresh per right-click and unparented when it closes:
    // a stock ActionRow has no dispose hook, so a popover parented for the row's
    // whole life leaks and warns at finalize. The action group is inserted on
    // the popover itself so the menu items resolve against it directly.
    let secondary = gtk::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    secondary.connect_pressed(glib::clone!(
        #[weak]
        row,
        #[strong]
        group,
        #[strong]
        menu,
        move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let popover = gtk::PopoverMenu::from_model(Some(&menu));
            popover.insert_action_group("menu", Some(&group));
            popover.set_parent(&row);
            popover.set_has_arrow(false);
            popover.set_halign(gtk::Align::Start);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
        }
    ));
    row.add_controller(secondary);

    // Middle-click opens the thread in a background tab, matching the web
    // convention of middle-clicking a link.
    let middle = gtk::GestureClick::new();
    middle.set_button(gdk::BUTTON_MIDDLE);
    middle.connect_pressed(glib::clone!(
        #[weak]
        row,
        #[strong]
        list,
        #[strong]
        message_id,
        move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            open_in_new_tab(&row, &list, &message_id);
        }
    ));
    row.add_controller(middle);
}

/// Walk up from a thread row to the enclosing TabView and open the thread in a
/// new background tab there.
fn open_in_new_tab(widget: &impl IsA<gtk::Widget>, list: &str, message_id: &str) {
    if let Some(tab_view) = widget
        .ancestor(adw::TabView::static_type())
        .and_downcast::<adw::TabView>()
    {
        crate::open_thread_in_new_tab(&tab_view, list, message_id);
    }
}

/// "Jul 3" for dates in the current year, "Jul 3 2019" otherwise.
pub fn format_date(date: &glib::DateTime) -> String {
    let same_year = glib::DateTime::now_local().is_ok_and(|now| now.year() == date.year());
    let format = if same_year { "%b %-e" } else { "%b %-e %Y" };
    date.format(format).map(Into::into).unwrap_or_default()
}
