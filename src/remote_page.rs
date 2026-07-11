use adw::prelude::*;
use gtk::{gio, glib};

use crate::lore;

/// Loading / content / error switcher shared by every page backed by a
/// network fetch. Destroying the widget (e.g. popping its page off the
/// navigation stack) cancels the in-flight request.
///
/// The loading spinner is an overlay ABOVE the content stack, not a stack
/// page: content handed over via show_content_covered is allocated (and can
/// lay itself out, fill, validate) underneath the still-spinning cover, and
/// reveal() lifts the cover once the content is actually presentable. One
/// spinner covers the whole journey from request to first complete frame.
#[derive(Clone)]
pub struct RemoteContent {
    root: gtk::Overlay,
    stack: gtk::Stack,
    cover: adw::Bin,
    cancellable: gio::Cancellable,
}

/// Weak handle for closures that live inside the widget tree itself (like
/// the error page's Retry button or the content's reveal trigger): a strong
/// `RemoteContent` there would keep the tree alive in a reference cycle.
pub struct RemoteContentWeak {
    root: glib::WeakRef<gtk::Overlay>,
    stack: glib::WeakRef<gtk::Stack>,
    cover: glib::WeakRef<adw::Bin>,
    cancellable: gio::Cancellable,
}

impl RemoteContentWeak {
    pub fn upgrade(&self) -> Option<RemoteContent> {
        Some(RemoteContent {
            root: self.root.upgrade()?,
            stack: self.stack.upgrade()?,
            cover: self.cover.upgrade()?,
            cancellable: self.cancellable.clone(),
        })
    }
}

impl RemoteContent {
    pub fn new() -> Self {
        let spinner = adw::Spinner::builder()
            .width_request(48)
            .height_request(48)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .build();
        // The cover must be opaque (stock .background) so half-built content
        // underneath never shows through.
        let cover = adw::Bin::builder()
            .css_classes(["background"])
            .child(&spinner)
            .build();

        // No transition: a GtkStack with one set clips children to its
        // rectangular bounds, cutting the card shadow of boxed lists down to
        // odd little arcs in the rounded-corner notches.
        let stack = gtk::Stack::builder().vexpand(true).build();

        let root = gtk::Overlay::builder().child(&stack).build();
        root.add_overlay(&cover);

        let cancellable = gio::Cancellable::new();
        root.connect_destroy(glib::clone!(
            #[strong]
            cancellable,
            move |_| cancellable.cancel()
        ));

        Self {
            root,
            stack,
            cover,
            cancellable,
        }
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.root
    }

    pub fn cancellable(&self) -> gio::Cancellable {
        self.cancellable.clone()
    }

    pub fn downgrade(&self) -> RemoteContentWeak {
        RemoteContentWeak {
            root: self.root.downgrade(),
            stack: self.stack.downgrade(),
            cover: self.cover.downgrade(),
            cancellable: self.cancellable.clone(),
        }
    }

    pub fn show_loading(&self) {
        self.cover.set_visible(true);
    }

    /// Show content that is presentable as-is.
    pub fn show_content(&self, child: &impl IsA<gtk::Widget>) {
        self.swap_child("content", child);
        self.reveal();
    }

    /// Mount content underneath the still-visible loading cover, so it can
    /// lay out and prepare itself while the spinner keeps spinning. The
    /// caller lifts the cover with reveal() once the content is ready.
    pub fn show_content_covered(&self, child: &impl IsA<gtk::Widget>) {
        self.swap_child("content", child);
    }

    /// Lift the loading cover. Idempotent.
    pub fn reveal(&self) {
        self.cover.set_visible(false);
    }

    pub fn show_error(&self, error: &lore::Error, on_retry: impl Fn() + 'static) {
        let message = error.to_string();

        // Carry the error text in a capped label rather than the status page's
        // own description: an unbounded message (a long request URL, say) would
        // otherwise grow the page tall enough to need scrolling and shove the
        // Retry button out of view. Two lines then ellipsis, full text on hover.
        let detail = gtk::Label::builder()
            .label(&message)
            .justify(gtk::Justification::Center)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .tooltip_text(&message)
            .css_classes(["dim-label"])
            .build();

        let retry = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        retry.connect_clicked(move |_| on_retry());

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .halign(gtk::Align::Center)
            .build();
        content.append(&detail);
        content.append(&retry);

        let status = adw::StatusPage::builder()
            .icon_name("network-error-symbolic")
            .title("Couldn't Reach lore.kernel.org")
            .child(&content)
            .build();

        self.swap_child("error", &status);
        self.reveal();
    }

    fn swap_child(&self, name: &str, child: &impl IsA<gtk::Widget>) {
        if let Some(old) = self.stack.child_by_name(name) {
            self.stack.remove(&old);
        }
        self.stack.add_named(child, Some(name));
        self.stack.set_visible_child_name(name);
    }
}
