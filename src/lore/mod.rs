mod feed;
mod manifest;

pub use feed::{PAGE_SIZE, Sort, ThreadSummary, fetch_thread_roots, search};
pub use manifest::{Inbox, fetch_inboxes};

use std::fmt;
use std::io::Read;

use gtk::{gio, glib};
use soup::prelude::*;

pub const BASE_URL: &str = "https://lore.kernel.org";
// lore.kernel.org returns 403 for requests without a User-Agent.
pub const USER_AGENT: &str = concat!("koshi/", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub enum Error {
    Http(glib::Error),
    Status(soup::Status),
    Gunzip(std::io::Error),
    Parse(String),
    Cancelled,
}

impl Error {
    pub fn is_cancelled(&self) -> bool {
        match self {
            Error::Cancelled => true,
            Error::Http(err) => err.matches(gio::IOErrorEnum::Cancelled),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(err) => write!(f, "{err}"),
            Error::Status(status) => {
                write!(f, "lore.kernel.org replied with {status:?}")
            }
            Error::Gunzip(err) => write!(f, "Could not decompress the response: {err}"),
            Error::Parse(msg) => write!(f, "Could not parse the response: {msg}"),
            Error::Cancelled => write!(f, "Cancelled"),
        }
    }
}

thread_local! {
    static SESSION: soup::Session = soup::Session::builder()
        .user_agent(USER_AGENT)
        .build();
}

pub async fn fetch(url: &str, cancellable: &gio::Cancellable) -> Result<glib::Bytes, Error> {
    let message = soup::Message::new("GET", url)
        .map_err(|err| Error::Parse(format!("bad URL {url}: {err}")))?;
    let request = SESSION.with(|s| s.send_and_read_future(&message, glib::Priority::DEFAULT));
    let bytes = gio::CancellableFuture::new(request, cancellable.clone())
        .await
        .map_err(|_| Error::Cancelled)?
        .map_err(Error::Http)?;
    if message.status() != soup::Status::Ok {
        return Err(Error::Status(message.status()));
    }
    Ok(bytes)
}

/// Decompress an `application/gzip` payload (lore's `.gz` endpoints are
/// gzipped files, not transport encoding, so libsoup does not decode them).
pub fn gunzip_to_string(bytes: &[u8]) -> Result<String, Error> {
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut decoded)
        .map_err(Error::Gunzip)?;
    Ok(String::from_utf8_lossy(&decoded).into_owned())
}

/// Fetch a whole thread as mboxrd text, given any message in it.
/// The Message-ID is accepted with or without angle brackets. The pseudo-list
/// `r` resolves a Message-ID across every list via lore's redirect.
pub async fn fetch_thread_mbox(
    list: &str,
    message_id: &str,
    cancellable: &gio::Cancellable,
) -> Result<String, Error> {
    let trimmed = message_id.trim().trim_matches(['<', '>']);
    let escaped = glib::Uri::escape_string(trimmed, None, true);
    let url = format!("{BASE_URL}/{list}/{escaped}/t.mbox.gz");
    let bytes = fetch(&url, cancellable).await?;
    gunzip_to_string(&bytes)
}
