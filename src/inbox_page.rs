use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::list_page::build_list_page;
use crate::lore::{self, Inbox};
use crate::remote_page::RemoteContent;
use crate::thread_list_page::{build_thread_list_page, format_date};

pub const INBOX_LIST_TITLE: &str = "Public Inboxes";

pub fn build_inbox_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let remote = RemoteContent::new();
    let page = build_list_page(
        INBOX_LIST_TITLE,
        "Open a public inbox",
        "Every list mirrored on lore.kernel.org. Pick one to open it in this tab.",
        &[],
        remote.widget(),
    );
    load(remote, nav.clone());
    page
}

fn load(remote: RemoteContent, nav: adw::NavigationView) {
    remote.show_loading();
    let cancellable = remote.cancellable();
    glib::spawn_future_local(async move {
        match lore::fetch_inboxes(&cancellable).await {
            Ok(inboxes) => remote.show_content(&build_inbox_list(&nav, inboxes)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                let weak = remote.downgrade();
                let nav = nav.downgrade();
                remote.show_error(&error, move || {
                    if let (Some(remote), Some(nav)) = (weak.upgrade(), nav.upgrade()) {
                        load(remote, nav);
                    }
                });
            }
        }
    });
}

fn build_inbox_list(nav: &adw::NavigationView, inboxes: Vec<Inbox>) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    for inbox in &inboxes {
        list.append(&build_inbox_row(inbox));
    }

    let inboxes = Rc::new(inboxes);
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let inbox = &inboxes[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &inbox.slug, &inbox.description));
        }
    ));

    list
}

fn build_inbox_row(inbox: &Inbox) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&inbox.slug))
        .subtitle(glib::markup_escape_text(&inbox.description))
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("mail-unread-symbolic"));
    // Last-activity stamp from the manifest (the `all` pseudo-inbox has none).
    if inbox.modified > 0
        && let Ok(date) = glib::DateTime::from_unix_local(inbox.modified)
    {
        row.add_suffix(
            &gtk::Label::builder()
                .label(format_date(&date))
                .css_classes(["dim-label", "numeric"])
                .build(),
        );
    }
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}
