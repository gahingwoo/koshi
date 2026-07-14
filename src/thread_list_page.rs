use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::list_page::{RowMenu, build_list_page};
use crate::lore::{self, Sort, ThreadSummary};
use crate::remote_page::{RemoteContent, When, load_on_first_show};
use crate::thread_page::{build_thread_page, launch_uri};

/// Rows shown per Load More step. lore hands over 200 threads per fetch, but
/// hundreds of live rows make every switch back to the tab re-shape all their
/// labels (Pango/fontconfig work, a stalled frame or three per switch — worse
/// the more such tabs are open), so the fetched tail stays unbuilt until the
/// user asks for it: Load More reveals locally first and only fetches once
/// everything fetched is showing.
const VISIBLE_CHUNK: usize = 50;

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

    /// Whether to nest replies under their parent. List browsing groups a patch
    /// series beneath its cover letter; search stays flat, matching lore's
    /// (ungrouped) search results.
    fn groups(&self) -> bool {
        matches!(self, Mode::Recent { .. })
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
        Mode::Recent {
            list: inbox_name.to_string(),
        },
        inbox_name,
        inbox_name,
        inbox_description,
        When::Now,
    )
}

/// The list page for a back stack the user has not navigated to (a thread
/// opened straight into its own tab). It fetches the list's feed only once
/// the user actually goes back to it.
pub fn build_thread_list_page_deferred(
    nav: &adw::NavigationView,
    inbox_name: &str,
) -> adw::NavigationPage {
    build_page(
        nav,
        Mode::Recent {
            list: inbox_name.to_string(),
        },
        inbox_name,
        inbox_name,
        "",
        When::OnFirstShow,
    )
}

pub fn build_search_page(nav: &adw::NavigationView, query: &str) -> adw::NavigationPage {
    build_page(
        nav,
        Mode::Search {
            list: "all".to_string(),
            query: query.to_string(),
        },
        &format!("Search: {query}"),
        "Search results",
        &format!("Matches for “{query}” across all of lore.kernel.org"),
        When::Now,
    )
}

fn build_page(
    nav: &adw::NavigationView,
    mode: Mode,
    page_title: &str,
    heading: &str,
    description: &str,
    when: When,
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

    match when {
        When::Now => load(remote, nav.clone(), mode, sort.get()),
        When::OnFirstShow => load_on_first_show(
            &page,
            glib::clone!(
                #[strong]
                remote,
                #[strong]
                nav,
                #[strong]
                mode,
                #[strong]
                sort,
                move || load(remote.clone(), nav.clone(), mode.clone(), sort.get())
            ),
        ),
    }
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

/// Pixels of extra indent per nesting level for a grouped reply.
const INDENT_STEP: i32 = 24;
/// Indent stops growing past this depth, so a deeply threaded series can't
/// march its rows off the right edge.
const MAX_DEPTH: usize = 6;

/// One list row: a thread summary plus its nesting depth. Depth 0 is a thread
/// root or standalone message; a greater depth is a reply (a patch under its
/// cover letter) shown indented and dimmed beneath its parent.
struct Row {
    thread: ThreadSummary,
    depth: usize,
}

/// Arrange a freshly fetched page into display order. When `group` is set (list
/// browsing), a message whose parent is *also on this page* is nested beneath
/// it, so a patch series collapses under its cover letter; siblings are ordered
/// oldest-first so a series reads 0, 1, 2, … Each group is emitted where its
/// newest member first appears, so groups keep lore's newest-first order and
/// already-shown rows never reshuffle when a later page arrives. When `group`
/// is unset (search), rows stay flat in lore's order.
///
/// Grouping is deliberately page-local and pointer-only: it never fabricates a
/// missing parent and never reaches across a page boundary. A series split by
/// pagination simply shows its overflow as plain rows rather than misgrouping.
fn arrange(threads: Vec<ThreadSummary>, group: bool) -> Vec<Row> {
    if !group {
        return threads
            .into_iter()
            .map(|thread| Row { thread, depth: 0 })
            .collect();
    }

    let n = threads.len();
    // Message-ID -> first index on this page.
    let mut index_of = std::collections::HashMap::with_capacity(n);
    for (i, thread) in threads.iter().enumerate() {
        index_of.entry(thread.message_id.as_str()).or_insert(i);
    }
    // parent[i] = index of i's in-reply-to target on this page, if present and
    // not i itself.
    let parent: Vec<Option<usize>> = threads
        .iter()
        .enumerate()
        .map(|(i, thread)| {
            let pid = thread.in_reply_to.as_deref()?;
            let p = *index_of.get(pid)?;
            (p != i).then_some(p)
        })
        .collect();
    // Children of each message, ordered oldest-first (lore lists newest-first,
    // which would otherwise reverse a series into N, …, 1, 0).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = *p {
            children[p].push(i);
        }
    }
    for kids in &mut children {
        kids.sort_by_key(|&c| threads[c].updated.to_unix());
    }

    // Emit each group's whole subtree the first time any member is reached
    // (scanning in lore's order), depth-first from the root.
    let mut slots: Vec<Option<ThreadSummary>> = threads.into_iter().map(Some).collect();
    let mut emitted = vec![false; n];
    let mut rows = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for start in 0..n {
        let root = root_of(start, &parent);
        if emitted[root] {
            continue;
        }
        stack.push((root, 0));
        while let Some((i, depth)) = stack.pop() {
            if emitted[i] {
                continue;
            }
            emitted[i] = true;
            if let Some(thread) = slots[i].take() {
                rows.push(Row { thread, depth });
            }
            // Reversed so the sorted children pop back in oldest-first order.
            for &c in children[i].iter().rev() {
                if !emitted[c] {
                    stack.push((c, depth + 1));
                }
            }
        }
    }
    // Safety net for any node stranded in a reference cycle (never reached from
    // an acyclic root): surface it as its own row rather than dropping it.
    for slot in &mut slots {
        if let Some(thread) = slot.take() {
            rows.push(Row { thread, depth: 0 });
        }
    }
    rows
}

/// Walk in-reply-to pointers up to the thread root, bounded by the page length
/// so a pathological cycle terminates instead of looping forever.
fn root_of(mut i: usize, parent: &[Option<usize>]) -> usize {
    for _ in 0..parent.len() {
        match parent[i] {
            Some(p) => i = p,
            None => break,
        }
    }
    i
}

fn build_thread_list(
    nav: &adw::NavigationView,
    cancellable: &gio::Cancellable,
    mode: &Mode,
    sort: Sort,
    threads: Vec<ThreadSummary>,
) -> gtk::Box {
    // The box exists to host the shared context-menu popover: RowMenu must not
    // parent it to the ListBox itself (see RowMenu's docs).
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        // The list sits inside a vexpanding stack; without this it stretches
        // and the boxed-list shadow outlines the empty space below the rows.
        .valign(gtk::Align::Start)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    container.append(&list);

    let menu = RowMenu::new(&container, &["Open in New _Tab", "Open on _Web"]);

    // lore has nothing further once it answers with a partial page (checked on
    // the raw count, which arranging into groups preserves).
    let exhausted = threads.len() < lore::PAGE_SIZE;
    let rows = arrange(threads, mode.groups());

    let shown = rows.len().min(VISIBLE_CHUNK);
    for row in &rows[..shown] {
        list.append(&build_thread_row(row, &menu, mode.list()));
    }
    if shown < rows.len() || !exhausted {
        list.append(&build_load_more_row());
    }

    let shown = Rc::new(Cell::new(shown));
    let exhausted = Rc::new(Cell::new(exhausted));
    let threads = Rc::new(RefCell::new(rows));
    // The cancellable is captured instead of the whole RemoteContent: this
    // closure lives inside the stack, and a strong stack reference here would
    // be a leaky cycle.
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        #[strong]
        threads,
        #[strong]
        shown,
        #[strong]
        exhausted,
        #[strong]
        cancellable,
        #[strong]
        mode,
        #[strong]
        menu,
        move |list, row| {
            let index = row.index() as usize;
            if index < shown.get() {
                let message_id = threads.borrow()[index].thread.message_id.clone();
                nav.push(&build_thread_page(&nav, mode.list(), &message_id));
            } else {
                load_more(
                    list,
                    row,
                    &menu,
                    threads.clone(),
                    shown.clone(),
                    exhausted.clone(),
                    mode.clone(),
                    sort,
                    cancellable.clone(),
                );
            }
        }
    ));

    container
}

/// The Load More row: reveal the next chunk of already-fetched threads, or —
/// once everything fetched is showing — fetch the next page first.
fn load_more(
    list: &gtk::ListBox,
    row: &gtk::ListBoxRow,
    menu: &RowMenu,
    threads: Rc<RefCell<Vec<Row>>>,
    shown: Rc<Cell<usize>>,
    exhausted: Rc<Cell<bool>>,
    mode: Mode,
    sort: Sort,
    cancellable: gio::Cancellable,
) {
    if shown.get() < threads.borrow().len() {
        reveal_chunk(
            list,
            row,
            menu,
            &threads.borrow(),
            &shown,
            &exhausted,
            mode.list(),
        );
        return;
    }

    if !row.is_sensitive() {
        return; // already loading
    }
    row.set_sensitive(false);
    let list = list.clone();
    let row = row.clone();
    let menu = menu.clone();
    glib::spawn_future_local(async move {
        let offset = threads.borrow().len();
        match mode.fetch(offset, sort, &cancellable).await {
            Ok(more) => {
                row.set_sensitive(true);
                exhausted.set(more.len() < lore::PAGE_SIZE);
                // Group the new page on its own; groups never cross a page
                // boundary, so this never disturbs rows already on screen.
                threads.borrow_mut().extend(arrange(more, mode.groups()));
                reveal_chunk(
                    &list,
                    &row,
                    &menu,
                    &threads.borrow(),
                    &shown,
                    &exhausted,
                    mode.list(),
                );
            }
            Err(error) if error.is_cancelled() => {}
            // Make the row clickable again; activating it retries.
            Err(_) => row.set_sensitive(true),
        }
    });
}

/// Insert the next chunk's rows above the Load More row; the row itself goes
/// away once there is nothing left to reveal or fetch.
fn reveal_chunk(
    list: &gtk::ListBox,
    load_more_row: &gtk::ListBoxRow,
    menu: &RowMenu,
    threads: &[Row],
    shown: &Cell<usize>,
    exhausted: &Cell<bool>,
    slug: &str,
) {
    let start = shown.get();
    let end = threads.len().min(start + VISIBLE_CHUNK);
    for row in &threads[start..end] {
        list.insert(&build_thread_row(row, menu, slug), load_more_row.index());
    }
    shown.set(end);
    if end == threads.len() && exhausted.get() {
        list.remove(load_more_row);
    }
}

fn build_load_more_row() -> adw::ButtonRow {
    adw::ButtonRow::builder().title("Load More").build()
}

fn build_thread_row(row_data: &Row, menu: &RowMenu, list: &str) -> adw::ActionRow {
    let thread = &row_data.thread;
    let row = adw::ActionRow::builder()
        .title(format!(
            "<tt>{}</tt>",
            glib::markup_escape_text(&thread.subject)
        ))
        .title_lines(1)
        .tooltip_text(&thread.subject)
        .subtitle(glib::markup_escape_text(&thread.author))
        .subtitle_lines(1)
        .activatable(true)
        .build();

    // A nested reply (a patch under its cover) is indented and dimmed with the
    // stock `dim-label` class, and carries a corner connector glyph tying it to
    // the parent above — so a series reads as one group without hiding any part
    // of it. The prefix indents one step per level (bar the connector's own),
    // and the row's dim-label class dims the glyph along with the text.
    if row_data.depth > 0 {
        row.add_css_class("dim-label");
        let depth = row_data.depth.min(MAX_DEPTH) as i32;
        let prefix = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request((depth - 1) * INDENT_STEP, -1);
        prefix.append(&spacer);
        prefix.append(
            &gtk::Label::builder()
                .label("↳")
                .valign(gtk::Align::Center)
                .margin_start(16)
                .tooltip_text("Reply in this thread")
                .build(),
        );
        row.add_prefix(&prefix);
    }

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

    add_row_actions(&row, menu, list, &thread.message_id);

    row
}

/// Wire a thread row's secondary-click context menu ("Open in New Tab", "Open
/// on Web" — handlers in the shared menu's label order) and its middle-click
/// shortcut for opening the thread in a background tab.
fn add_row_actions(row: &adw::ActionRow, menu: &RowMenu, list: &str, message_id: &str) {
    // `message_id` arrives already stripped of angle brackets, but trim to be
    // safe and match the /r/ redirect URL used in the message reading view.
    let bare = message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let url = format!("https://lore.kernel.org/r/{bare}/");
    let list = list.to_string();
    let message_id = message_id.to_string();

    menu.attach(
        row,
        vec![
            Rc::new(glib::clone!(
                #[weak]
                row,
                #[strong]
                list,
                #[strong]
                message_id,
                move || open_in_new_tab(&row, &list, &message_id)
            )) as Rc<dyn Fn()>,
            Rc::new(glib::clone!(
                #[weak]
                row,
                #[strong]
                url,
                move || launch_uri(&row, &url)
            )),
        ],
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary with the given Message-ID, optional parent, and a received
    /// time of `unix` seconds (drives sibling ordering).
    fn summary(id: &str, in_reply_to: Option<&str>, unix: i64) -> ThreadSummary {
        ThreadSummary {
            subject: id.to_string(),
            author: "someone".to_string(),
            updated: glib::DateTime::from_unix_utc(unix).unwrap(),
            message_id: id.to_string(),
            in_reply_to: in_reply_to.map(str::to_string),
        }
    }

    fn shape(rows: &[Row]) -> Vec<(&str, usize)> {
        rows.iter()
            .map(|r| (r.thread.subject.as_str(), r.depth))
            .collect()
    }

    #[test]
    fn ungrouped_keeps_order_and_flat_depth() {
        // Search mode: even a reply keeps lore's order and stays at depth 0.
        let page = vec![
            summary("a", None, 3),
            summary("b", Some("a"), 2),
            summary("c", None, 1),
        ];
        assert_eq!(
            shape(&arrange(page, false)),
            vec![("a", 0), ("b", 0), ("c", 0)]
        );
    }

    #[test]
    fn shallow_series_nests_under_cover_oldest_first() {
        // lore returns newest-first: 2/2, 1/2, then the 0/2 cover (oldest).
        // All patches reply to the cover.
        let page = vec![
            summary("p2", Some("cover"), 30),
            summary("p1", Some("cover"), 20),
            summary("cover", None, 10),
        ];
        // Cover on top, patches indented and in send order.
        assert_eq!(
            shape(&arrange(page, true)),
            vec![("cover", 0), ("p1", 1), ("p2", 1)]
        );
    }

    #[test]
    fn deep_thread_indents_stepwise() {
        // 0 <- 1 <- 2 chain (each replies to the previous).
        let page = vec![
            summary("m2", Some("m1"), 30),
            summary("m1", Some("m0"), 20),
            summary("m0", None, 10),
        ];
        assert_eq!(
            shape(&arrange(page, true)),
            vec![("m0", 0), ("m1", 1), ("m2", 2)]
        );
    }

    #[test]
    fn missing_parent_stays_a_root() {
        // The cover is not on this page; its child cannot nest and stays flat.
        let page = vec![summary("orphan", Some("absent-cover"), 10)];
        assert_eq!(shape(&arrange(page, true)), vec![("orphan", 0)]);
    }

    #[test]
    fn independent_groups_keep_newest_first_order() {
        // Two series interleaved by time; each group is anchored where its
        // newest member appears, so the newer group leads.
        let page = vec![
            summary("bp1", Some("bcover"), 50), // newer series' patch
            summary("bcover", None, 40),
            summary("acover", None, 20),
            summary("ap1", Some("acover"), 25),
        ];
        assert_eq!(
            shape(&arrange(page, true)),
            vec![("bcover", 0), ("bp1", 1), ("acover", 0), ("ap1", 1)]
        );
    }

    #[test]
    fn self_referential_parent_does_not_loop() {
        let page = vec![summary("x", Some("x"), 10)];
        assert_eq!(shape(&arrange(page, true)), vec![("x", 0)]);
    }
}
