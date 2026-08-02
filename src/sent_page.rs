use adw::prelude::*;
use gtk::glib;

use crate::sent::{self, SentMessage};
use crate::thread_page::build_thread_page;

pub const SENT_TITLE: &str = "Sent";

/// Widget name marking the sent page, matching how [`crate::favorites_page`]
/// marks its own — recognizable without comparing titles or navigation tags.
pub const SENT_PAGE_NAME: &str = "koshi-sent-page";

pub fn build_sent_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let sent = sent::all();

    // HIG placeholder-page pattern: an empty view gets a symbolic
    // AdwStatusPage instead of an empty list.
    let page = if sent.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("mail-reply-sender-symbolic")
            .title("No Sent Messages")
            .description("Replies you send are logged here")
            .build();
        adw::NavigationPage::new(&status, SENT_TITLE)
    } else {
        let list = build_sent_list(nav, sent);
        crate::list_page::build_list_page(
            SENT_TITLE,
            SENT_TITLE,
            "Replies you've sent, most recent first",
            &[],
            &list,
        )
    };
    page.set_widget_name(SENT_PAGE_NAME);
    page
}

fn build_sent_list(nav: &adw::NavigationView, sent: Vec<SentMessage>) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for msg in &sent {
        list.append(&build_sent_row(msg));
    }
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let msg = &sent[row.index() as usize];
            // Only a reply to a thread Koshi had open can be reopened this
            // way; a from-scratch compose has nowhere to go back to.
            if let (Some(list_slug), Some(message_id)) =
                (msg.list.as_deref(), msg.thread_message_id.as_deref())
            {
                nav.push(&build_thread_page(&nav, list_slug, message_id));
            }
        }
    ));
    list
}

fn build_sent_row(msg: &SentMessage) -> adw::ActionRow {
    let has_thread = msg.list.is_some() && msg.thread_message_id.is_some();
    let row = adw::ActionRow::builder()
        .title(format!(
            "<tt>{}</tt>",
            glib::markup_escape_text(&msg.subject)
        ))
        .title_lines(1)
        .subtitle(glib::markup_escape_text(&msg.to))
        .subtitle_lines(1)
        .tooltip_text(&msg.subject)
        .activatable(has_thread)
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label(&msg.sent_at)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );
    if has_thread {
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    }
    row
}
