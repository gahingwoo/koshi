use gtk::{gio, glib};
use quick_xml::Reader;
use quick_xml::events::Event;

use super::{BASE_URL, Error, fetch};

pub struct ThreadSummary {
    pub subject: String,
    pub author: String,
    pub updated: glib::DateTime,
    /// Without angle brackets.
    pub message_id: String,
    /// Message-ID this entry replies to, taken from the Atom `thr:in-reply-to`
    /// element (without angle brackets); `None` for a thread root. Used to nest
    /// patch-series parts under their cover letter when browsing a list.
    pub in_reply_to: Option<String>,
}

/// Entries per Atom results page (fixed by lore; paginate with `o=`).
pub const PAGE_SIZE: usize = 200;

/// Result ordering for a search. lore (public-inbox) sorts by received time,
/// newest first, unless the `r` flag is present, which switches to Xapian
/// relevance ranking.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Date,
    Relevance,
}

impl Sort {
    /// The query-string fragment that selects this ordering.
    fn query_flag(self) -> &'static str {
        match self {
            Sort::Date => "",
            Sort::Relevance => "&r",
        }
    }
}

/// Recent thread roots of a list, newest first. lore rejects a lone
/// `NOT s:"Re:"`, so an always-true received-time clause is prepended
/// (the same trick kw's patch-hub uses).
pub async fn fetch_thread_roots(
    list: &str,
    offset: usize,
    cancellable: &gio::Cancellable,
) -> Result<Vec<ThreadSummary>, Error> {
    let url = format!("{BASE_URL}/{list}/?q=rt%3A..+AND+NOT+s%3A%22Re%3A%22&x=A&o={offset}");
    let bytes = fetch(&url, cancellable).await?;
    parse_atom(&String::from_utf8_lossy(&bytes))
}

/// Full-text search over a list (or the `all` pseudo-list), using lore's
/// Xapian query syntax (`s:`, `f:`, `b:`, AND/OR/NOT, ...).
pub async fn search(
    list: &str,
    query: &str,
    offset: usize,
    sort: Sort,
    cancellable: &gio::Cancellable,
) -> Result<Vec<ThreadSummary>, Error> {
    let escaped = glib::Uri::escape_string(query, None, true);
    let url = format!(
        "{BASE_URL}/{list}/?q={escaped}&x=A&o={offset}{}",
        sort.query_flag()
    );
    let bytes = fetch(&url, cancellable).await?;
    parse_atom(&String::from_utf8_lossy(&bytes))
}

#[derive(Default)]
struct EntryBuilder {
    title: String,
    name: String,
    email: String,
    updated: String,
    link: Option<String>,
    in_reply_to: Option<String>,
}

impl EntryBuilder {
    fn build(self) -> Option<ThreadSummary> {
        let updated = glib::DateTime::from_iso8601(&self.updated, None)
            .ok()?
            .to_local()
            .ok()?;
        Some(ThreadSummary {
            subject: self.title,
            author: if self.name.is_empty() {
                self.email
            } else {
                self.name
            },
            updated,
            message_id: message_id_from_url(&self.link?)?,
            in_reply_to: self.in_reply_to,
        })
    }
}

impl EntryBuilder {
    fn field_mut(&mut self, field: &Field) -> &mut String {
        match field {
            Field::Title => &mut self.title,
            Field::Name => &mut self.name,
            Field::Email => &mut self.email,
            Field::Updated => &mut self.updated,
        }
    }
}

enum Field {
    Title,
    Name,
    Email,
    Updated,
}

/// Resolve the XML predefined entities and numeric character references.
fn resolve_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// The last path segment of a lore message URL is the Message-ID.
fn message_id_from_url(url: &str) -> Option<String> {
    let id = url.trim_end_matches('/').rsplit('/').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Parse a lore Atom feed (search results or new.atom) into thread summaries.
/// Elements are matched by local name; the `<content>` subtree (an XHTML copy
/// of the message body) is skipped wholesale.
pub fn parse_atom(xml: &str) -> Result<Vec<ThreadSummary>, Error> {
    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();
    let mut entry: Option<EntryBuilder> = None;
    let mut text_target: Option<Field> = None;
    let mut content_depth = 0u32;

    loop {
        let event = reader
            .read_event()
            .map_err(|err| Error::Parse(err.to_string()))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let local = e.local_name();
                let empty = matches!(event, Event::Empty(_));
                if content_depth > 0 {
                    if !empty {
                        content_depth += 1;
                    }
                    continue;
                }
                match local.as_ref() {
                    b"entry" if !empty => entry = Some(EntryBuilder::default()),
                    b"content" if !empty => content_depth = 1,
                    b"title" => text_target = entry.is_some().then_some(Field::Title),
                    b"name" => text_target = entry.is_some().then_some(Field::Name),
                    b"email" => text_target = entry.is_some().then_some(Field::Email),
                    b"updated" => text_target = entry.is_some().then_some(Field::Updated),
                    b"link" => {
                        if let Some(entry) = entry.as_mut() {
                            entry.link = attribute(e, b"href");
                        }
                    }
                    // <thr:in-reply-to href=".../parent-msg-id/"> — the parent's
                    // own href, kept separate from the entry's <link> so it can
                    // seed threading without clobbering the entry's Message-ID.
                    b"in-reply-to" => {
                        if let Some(entry) = entry.as_mut() {
                            entry.in_reply_to = attribute(e, b"href")
                                .as_deref()
                                .and_then(message_id_from_url);
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref t) => {
                if content_depth > 0 {
                    continue;
                }
                if let (Some(entry), Some(field)) = (entry.as_mut(), text_target.as_ref()) {
                    let text = t.decode().map_err(|err| Error::Parse(err.to_string()))?;
                    entry.field_mut(field).push_str(&text);
                }
            }
            // Entity references (&amp; etc.) arrive as their own events.
            Event::GeneralRef(ref r) => {
                if content_depth > 0 {
                    continue;
                }
                if let (Some(entry), Some(field)) = (entry.as_mut(), text_target.as_ref()) {
                    let name = r.decode().map_err(|err| Error::Parse(err.to_string()))?;
                    if let Some(c) = resolve_entity(&name) {
                        entry.field_mut(field).push(c);
                    }
                }
            }
            Event::End(ref e) => {
                if content_depth > 0 {
                    content_depth -= 1;
                    continue;
                }
                text_target = None;
                if e.local_name().as_ref() == b"entry"
                    && let Some(summary) = entry.take().and_then(EntryBuilder::build)
                {
                    entries.push(summary);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(entries)
}

fn attribute(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    let attr = e.try_get_attribute(name).ok()??;
    let value = attr.unescape_value().ok()?;
    Some(value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real https://lore.kernel.org/lkml/?q=...&x=A response.
    const FEED: &str = r#"<?xml version="1.0" encoding="us-ascii"?>
<feed
xmlns="http://www.w3.org/2005/Atom"
xmlns:thr="http://purl.org/syndication/thread/1.0"><title>s:sched - search results</title><link
rel="alternate"
type="text/html"
href="https://lore.kernel.org/lkml/?q=s:sched"/><link
rel="self"
href="https://lore.kernel.org/lkml/?q=s:sched&amp;x=A"/><id>urn:uuid:e4e4cb11-6c40-b721-0545-7c87af9d104a</id><updated>2026-07-05T20:11:45Z</updated><entry><author><name>David Laight</name><email>david.laight.linux@gmail.com</email></author><title>Re: [PATCH RESEND] sched/mmcid: Use clamp() &amp; simplify</title><updated>2026-07-05T19:07:27Z</updated><link
href="https://lore.kernel.org/lkml/20260705200723.66564929@pumpkin/"/><id>urn:uuid:a46fa9e7-1c10-bd48-4176-d4e18055eb81</id><thr:in-reply-to
ref="urn:uuid:868f20c5-061c-53f7-b784-4dbf70271ffe"
href="https://lore.kernel.org/lkml/20260705172054.339425-2-thorsten.blum@linux.dev/"/><content
type="xhtml"><div
xmlns="http://www.w3.org/1999/xhtml"><pre
style="white-space:pre-wrap">Body text with a fake <span
class="q">&gt; title element: </span><a
href="https://example.com/not-the-link/">quoted</a>
</pre></div></content></entry><entry><author><email>anon@example.org</email></author><title>[PATCH] example: do a thing</title><updated>2026-07-05T18:00:00Z</updated><link
href="https://lore.kernel.org/lkml/some-id@example.org/"/><id>urn:uuid:00000000-0000-0000-0000-000000000000</id><content
type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><pre>hi</pre></div></content></entry></feed>"#;

    #[test]
    fn parses_entries_with_author_subject_and_message_id() {
        let entries = parse_atom(FEED).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].author, "David Laight");
        assert_eq!(
            entries[0].subject,
            "Re: [PATCH RESEND] sched/mmcid: Use clamp() & simplify"
        );
        assert_eq!(entries[0].message_id, "20260705200723.66564929@pumpkin");
    }

    #[test]
    fn thr_in_reply_to_href_never_becomes_the_message_link() {
        // The thr:in-reply-to element also carries an href; it must not
        // clobber the entry's own <link>.
        let entries = parse_atom(FEED).unwrap();
        assert_eq!(entries[0].message_id, "20260705200723.66564929@pumpkin");
    }

    #[test]
    fn in_reply_to_is_taken_from_thr_element_href() {
        let entries = parse_atom(FEED).unwrap();
        // entry[0] carries a thr:in-reply-to pointing at its parent's message.
        assert_eq!(
            entries[0].in_reply_to.as_deref(),
            Some("20260705172054.339425-2-thorsten.blum@linux.dev")
        );
        // entry[1] is a root: no thr:in-reply-to element.
        assert_eq!(entries[1].in_reply_to, None);
    }

    #[test]
    fn author_falls_back_to_email_when_name_is_missing() {
        let entries = parse_atom(FEED).unwrap();
        assert_eq!(entries[1].author, "anon@example.org");
    }

    #[test]
    fn timestamps_are_parsed() {
        let entries = parse_atom(FEED).unwrap();
        let utc = entries[0].updated.to_utc().unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").unwrap(), "2026-07-05 19:07");
    }

    #[test]
    fn message_body_content_does_not_leak_into_fields() {
        let entries = parse_atom(FEED).unwrap();
        assert!(!entries[0].subject.contains("Body text"));
        assert!(!entries[0].subject.contains("quoted"));
    }

    #[test]
    fn feed_level_title_and_links_are_ignored() {
        let entries = parse_atom(FEED).unwrap();
        assert_ne!(entries[0].subject, "s:sched - search results");
        assert_ne!(entries[0].message_id, "?q=s:sched");
    }
}
