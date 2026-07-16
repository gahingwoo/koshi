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
    /// The response exceeded the byte cap for its endpoint. Degenerate
    /// "threads" — chiefly the `rtt-probe` monitoring mails, whose shared
    /// subject lumps a quarter-million messages under one Message-ID — would
    /// otherwise pull hundreds of MB and stall the parser indefinitely.
    TooLarge,
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
            Error::TooLarge => write!(f, "This thread is too large to display"),
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

/// Chunk size for the streaming, size-capped fetch. Large enough to keep the
/// read loop cheap, small enough that the cap is enforced promptly.
const FETCH_CHUNK: usize = 64 * 1024;

/// Like [`fetch`], but streams the body and aborts with [`Error::TooLarge`]
/// once more than `max_bytes` have arrived, so a runaway endpoint can never
/// pull an unbounded amount into memory before we notice.
pub async fn fetch_capped(
    url: &str,
    max_bytes: usize,
    cancellable: &gio::Cancellable,
) -> Result<Vec<u8>, Error> {
    let message = soup::Message::new("GET", url)
        .map_err(|err| Error::Parse(format!("bad URL {url}: {err}")))?;
    let send = SESSION.with(|s| s.send_future(&message, glib::Priority::DEFAULT));
    let stream = gio::CancellableFuture::new(send, cancellable.clone())
        .await
        .map_err(|_| Error::Cancelled)?
        .map_err(Error::Http)?;
    if message.status() != soup::Status::Ok {
        return Err(Error::Status(message.status()));
    }

    let mut body = Vec::new();
    loop {
        let read = stream.read_bytes_future(FETCH_CHUNK, glib::Priority::DEFAULT);
        let chunk = gio::CancellableFuture::new(read, cancellable.clone())
            .await
            .map_err(|_| Error::Cancelled)?
            .map_err(Error::Http)?;
        if chunk.is_empty() {
            break;
        }
        if body.len() + chunk.len() > max_bytes {
            // Close the stream so libsoup stops pulling the rest down.
            let _ = stream.close(gio::Cancellable::NONE);
            return Err(Error::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Decompress an `application/gzip` payload (lore's `.gz` endpoints are
/// gzipped files, not transport encoding, so libsoup does not decode them).
pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut decoded)
        .map_err(Error::Gunzip)?;
    Ok(decoded)
}

/// Like [`gunzip`], but stops with [`Error::TooLarge`] once the inflated
/// output would exceed `max_bytes`. Guards against a compression bomb (or a
/// merely enormous thread) that fits under the download cap while compressed
/// yet balloons past what the parser can handle.
pub fn gunzip_capped(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, Error> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf).map_err(Error::Gunzip)?;
        if n == 0 {
            break;
        }
        if decoded.len() + n > max_bytes {
            return Err(Error::TooLarge);
        }
        decoded.extend_from_slice(&buf[..n]);
    }
    Ok(decoded)
}

pub fn gunzip_to_string(bytes: &[u8]) -> Result<String, Error> {
    Ok(String::from_utf8_lossy(&gunzip(bytes)?).into_owned())
}

/// Fetch a whole thread as mboxrd bytes, given any message in it.
/// Bytes, not text: individual messages carry their own charsets (KOI8-R
/// replies are alive and well on lkml), so any whole-file text conversion
/// here would corrupt them before the MIME parser can see the declaration.
/// The Message-ID is accepted with or without angle brackets. The pseudo-list
/// `r` resolves a Message-ID across every list via lore's redirect.
///
/// Consults the on-disk cache: a copy fetched within the last
/// [`crate::cache::FRESH_FOR_SECS`] is served without touching the network,
/// a successful download replaces the cached entry, and when the network
/// fails a stale copy (any age) is served instead of the error — a thread you
/// have read before stays readable offline. Callers that must see the live
/// thread use [`fetch_thread_mbox_live`].
///
/// Both the compressed download and its inflated form are size-capped: a real
/// thread — even a long patch series with full quoting — stays well under a
/// few MB, whereas the degenerate `rtt-probe` "thread" is ~140 MB gzipped and
/// ~580 MB inflated across a quarter-million messages. Exceeding either cap
/// yields [`Error::TooLarge`] instead of stalling on an unbounded parse.
pub async fn fetch_thread_mbox(
    list: &str,
    message_id: &str,
    cancellable: &gio::Cancellable,
) -> Result<Vec<u8>, Error> {
    fetch_thread_mbox_inner(list, message_id, true, cancellable).await
}

/// Like [`fetch_thread_mbox`], but always downloads — for the subscription
/// watcher, whose whole job is spotting messages the cache can't have yet,
/// and the thread page's Refresh button, whose whole point is bypassing the
/// fresh window. Still rewrites the cache entry on success (so watched
/// threads keep their cache warm), but never *reads* the cache: a poll that
/// can't reach lore reports its error rather than dressing up stale bytes as
/// a result.
pub async fn fetch_thread_mbox_live(
    list: &str,
    message_id: &str,
    cancellable: &gio::Cancellable,
) -> Result<Vec<u8>, Error> {
    fetch_thread_mbox_inner(list, message_id, false, cancellable).await
}

async fn fetch_thread_mbox_inner(
    list: &str,
    message_id: &str,
    use_cache: bool,
    cancellable: &gio::Cancellable,
) -> Result<Vec<u8>, Error> {
    let trimmed = message_id.trim().trim_matches(['<', '>']);
    // A fresh cached copy stands in for the network entirely. A cached entry
    // that fails to inflate (torn write, disk rot) is treated as absent.
    if use_cache
        && let Some(gz) = crate::cache::load_fresh(list, trimmed)
        && let Ok(mbox) = gunzip_capped(&gz, MAX_THREAD_INFLATED)
    {
        return Ok(mbox);
    }
    let escaped = glib::Uri::escape_string(trimmed, None, true);
    let url = format!("{BASE_URL}/{list}/{escaped}/t.mbox.gz");
    match fetch_capped(&url, MAX_THREAD_DOWNLOAD, cancellable).await {
        Ok(gz) => {
            // Cache only what inflates cleanly: an over-cap bomb is rejected
            // every time anyway, so there is no point storing it.
            let mbox = gunzip_capped(&gz, MAX_THREAD_INFLATED)?;
            crate::cache::store(list, trimmed, &gz);
            Ok(mbox)
        }
        Err(err) => {
            // Offline fallback: any cached copy, however old, beats an error
            // page — but a cancelled fetch means the page is gone, not that
            // the user should see old mail.
            if use_cache
                && !err.is_cancelled()
                && let Some(gz) = crate::cache::load_any(list, trimmed)
                && let Ok(mbox) = gunzip_capped(&gz, MAX_THREAD_INFLATED)
            {
                return Ok(mbox);
            }
            Err(err)
        }
    }
}

/// Byte cap on the gzipped `t.mbox.gz` download. Comfortably clears any
/// genuine thread while cutting off the ~140 MB `rtt-probe` pathology.
const MAX_THREAD_DOWNLOAD: usize = 32 * 1024 * 1024;

/// Byte cap on the inflated mbox handed to the parser. Guards against a highly
/// compressible payload that slips under the download cap yet inflates huge.
const MAX_THREAD_INFLATED: usize = 96 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gunzip_capped_returns_payload_within_cap() {
        let payload = b"From mboxrd@z Thu Jan  1 00:00:00 1970\n";
        let out = gunzip_capped(&gzip(payload), 1024).unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn gunzip_capped_rejects_payload_over_cap() {
        // Highly compressible: 1 MiB of zeros gzips to a few hundred bytes,
        // the exact bomb the cap exists to stop.
        let bomb = vec![0u8; 1024 * 1024];
        assert!(matches!(
            gunzip_capped(&gzip(&bomb), 4096),
            Err(Error::TooLarge)
        ));
    }
}
