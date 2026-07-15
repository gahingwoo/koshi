//! Assembling a reply into the RFC 5322 message Koshi hands to `git
//! send-email`, and parsing that message back when the user has edited it.
//!
//! Koshi does not forge the MIME machinery itself: `git send-email` writes the
//! authoritative `MIME-Version`, `Content-Type` (charset), and
//! `Content-Transfer-Encoding` on send, RFC 2047-encodes the `Subject`, and
//! stamps `Date` and `Message-ID` (the latter in our `--from` domain). So this
//! module only emits the headers git cannot invent — recipients, subject text,
//! and the threading chain — within the rules read from git's source:
//!
//! - A dedicated header (`In-Reply-To`, `References`) is honored only with
//!   exactly one space after the colon; anything else degrades to a duplicated
//!   pass-through. So every header here is `Name: value`.
//! - `References:` is dropped unless `In-Reply-To:` accompanies it, so the two
//!   are always emitted together.
//! - The body is normalised to LF: a CR would make git re-encode the whole
//!   message to quoted-printable.
//!
//! [`render`] builds the message Koshi prefills the composer with; the composer
//! is then the raw message, edited in place, so [`parse_headers`] reads it back
//! at send time for the `--from` address, the confirmation summary, and
//! validation.

use std::collections::HashSet;

/// References grow by one per reply; cap the chain so a deep thread cannot
/// produce an unbounded header. The root is always kept (it anchors threading)
/// along with the most recent ancestors.
const MAX_REFERENCES: usize = 25;

/// Continuation indent for a folded `Cc`: a newline plus four spaces, aligning
/// each address under the first (past `Cc: `). RFC 5322 §2.2.3 header folding is
/// transparent to a receiver — the value reassembles across the fold — and
/// [`unfold`] collapses it back for the recipients Koshi hands git.
const CC_FOLD_INDENT: &str = "\n    ";

/// Continuation indent for a folded `References`: a newline plus twelve spaces,
/// aligning each `<id>` under the first (past `References: `).
const REFERENCES_FOLD_INDENT: &str = "\n            ";

/// A reply ready to render: every header value already in its final form.
pub struct Outgoing {
    /// The `From:` — must equal what `git send-email` sends as (`--from`), or
    /// git rewrites the header and injects the original into the body.
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    /// The parent's Message-ID, as the composer holds it (bare or bracketed).
    pub in_reply_to: String,
    /// The parent message's `References` header, verbatim.
    pub references: String,
    /// The `User-Agent` to advertise, or `None` when the user has opted out of
    /// identifying Koshi in Preferences.
    pub user_agent: Option<String>,
    pub body: String,
}

/// Render `msg` as the raw message to prefill the composer with: the headers
/// Koshi controls, a blank line, then the normalised body. No MIME headers, no
/// `Date`, no `Message-ID` — `git send-email` writes all of those on send, and
/// RFC 2047-encodes the `Subject`, so the subject here stays human-readable.
pub fn render(msg: &Outgoing) -> String {
    let mut out = String::new();
    out.push_str(&format!("From: {}\n", msg.from.trim()));
    out.push_str(&format!("To: {}\n", msg.to.trim()));

    let cc = msg.cc.trim();
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {}\n", fold_addresses(cc)));
    }

    out.push_str(&format!("Subject: {}\n", msg.subject.trim()));

    // References travels only with In-Reply-To (git drops a lone one).
    if let Some(in_reply_to) = bracket_id(&msg.in_reply_to) {
        out.push_str(&format!("In-Reply-To: {in_reply_to}\n"));
        let references = build_references(&msg.references, &msg.in_reply_to);
        if !references.is_empty() {
            out.push_str(&format!(
                "References: {}\n",
                references.join(REFERENCES_FOLD_INDENT)
            ));
        }
    }

    if let Some(user_agent) = msg.user_agent.as_deref() {
        let user_agent = user_agent.trim();
        if !user_agent.is_empty() {
            out.push_str(&format!("User-Agent: {user_agent}\n"));
        }
    }

    out.push('\n');
    out.push_str(&normalize_body(&msg.body));
    out
}

/// Split a raw message into its header pairs and its body. The header block ends
/// at the first empty line; a line beginning with whitespace folds onto the
/// previous header's value (RFC 5322 folding). The body is everything after the
/// separating blank line, verbatim; a message with no blank line is all headers
/// and an empty body. Header names are trimmed but kept in their original case.
pub fn parse_headers(raw: &str) -> (Vec<(String, String)>, String) {
    let (head, body) = match raw.split_once("\n\n") {
        Some((head, body)) => (head, body.to_string()),
        None => (raw.trim_end_matches('\n'), String::new()),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.split('\n') {
        if (line.starts_with(' ') || line.starts_with('\t')) && !headers.is_empty() {
            let last = headers.last_mut().expect("checked non-empty");
            last.1.push('\n');
            last.1.push_str(line);
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    (headers, body)
}

/// The value of the first header named `name` (case-insensitive), if present.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Fold a comma-separated address list so each address sits on its own line —
/// the comma stays at the end of a line, each continuation indented per
/// [`FOLD_INDENT`]. RFC 5322 header folding, so the list reassembles unchanged
/// at the receiver.
fn fold_addresses(list: &str) -> String {
    list.split(',')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .collect::<Vec<_>>()
        .join(&format!(",{CC_FOLD_INDENT}"))
}

/// Collapse a folded header value back to a single line: continuation lines
/// (Koshi's or any RFC 5322 folding the user left) rejoin with one space, so a
/// folded `Cc`/`To` becomes the clean comma list Koshi hands git as `--cc`.
pub fn unfold(value: &str) -> String {
    value
        .split('\n')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The outgoing `References` chain: the parent's References followed by the
/// parent's Message-ID, each a single `<id>` token, capped. Empty when there
/// is no parent to reference.
fn build_references(parent_refs: &str, parent_msgid: &str) -> Vec<String> {
    let mut ids = reference_ids(parent_refs);
    if let Some(parent) = bare_id(parent_msgid) {
        // Avoid a duplicate tail if the parent already ends the chain.
        if ids.last() != Some(&parent) {
            ids.push(parent);
        }
    }
    cap_references(&ids, MAX_REFERENCES)
        .iter()
        .map(|id| format!("<{id}>"))
        .collect()
}

/// Trim a reference chain to at most `max` ids, keeping the root and the most
/// recent ancestors — the two ends that matter for threading.
fn cap_references(ids: &[String], max: usize) -> Vec<String> {
    if ids.len() <= max || ids.is_empty() {
        return ids.to_vec();
    }
    let mut kept = vec![ids[0].clone()];
    kept.extend_from_slice(&ids[ids.len() - (max - 1)..]);
    kept
}

/// Every `<id>` token in a References/In-Reply-To value, as bare ids. Falls
/// back to whitespace splitting when a value carries no brackets.
fn reference_ids(header: &str) -> Vec<String> {
    let header = header.trim();
    if header.is_empty() {
        return Vec::new();
    }
    if !header.contains('<') {
        return header.split_whitespace().map(str::to_string).collect();
    }
    let mut ids = Vec::new();
    let mut rest = header;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            break;
        };
        let id = after[..end].trim();
        if !id.is_empty() {
            ids.push(id.to_string());
        }
        rest = &after[end + 1..];
    }
    ids
}

/// The single bare id in a Message-ID value, if any.
fn bare_id(header: &str) -> Option<String> {
    reference_ids(header).into_iter().next()
}

/// Wrap a Message-ID in angle brackets for a header, or `None` when empty.
fn bracket_id(header: &str) -> Option<String> {
    bare_id(header).map(|id| format!("<{id}>"))
}

/// The body as git wants it: LF line endings — a CR would flip the whole
/// message to quoted-printable — and exactly one trailing newline.
fn normalize_body(body: &str) -> String {
    let mut out = body.replace("\r\n", "\n").replace('\r', "\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// The Cc list for a reply: `cc` with blanks, duplicates, and anyone already
/// in `to` removed. Addresses are compared case-insensitively on their `<addr>`
/// part, so `Name <a@b>` and `a@b` collapse to one.
pub fn dedup_cc(to: &str, cc: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(address_key(to));

    let mut out = Vec::new();
    for entry in cc {
        let entry = entry.trim();
        let key = address_key(entry);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(entry.to_string());
    }
    out
}

/// The comparison key for an address: the lowercased `<addr>` part, or the
/// lowercased whole when it has no brackets.
fn address_key(addr: &str) -> String {
    let inner = match (addr.find('<'), addr.rfind('>')) {
        (Some(start), Some(end)) if start < end => &addr[start + 1..end],
        _ => addr,
    };
    inner.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Outgoing {
        Outgoing {
            from: "Nika Krasnova <nika@nikableh.moe>".to_string(),
            to: "Maintainer <maint@kernel.org>".to_string(),
            cc: "list@vger.kernel.org".to_string(),
            subject: "Re: [PATCH] fix the thing".to_string(),
            in_reply_to: "<parent@kernel.org>".to_string(),
            references: "<root@kernel.org> <mid@kernel.org>".to_string(),
            user_agent: None,
            body: "Looks good to me.\n".to_string(),
        }
    }

    #[test]
    fn render_emits_threading_headers_without_mime_date_or_message_id() {
        let out = render(&sample());
        assert_eq!(
            out,
            concat!(
                "From: Nika Krasnova <nika@nikableh.moe>\n",
                "To: Maintainer <maint@kernel.org>\n",
                "Cc: list@vger.kernel.org\n",
                "Subject: Re: [PATCH] fix the thing\n",
                "In-Reply-To: <parent@kernel.org>\n",
                // References folds one <id> per line, aligned under the value
                // (12 spaces, past "References: ") — RFC 5322 §2.2.3.
                "References: <root@kernel.org>\n            <mid@kernel.org>\n            <parent@kernel.org>\n",
                "\n",
                "Looks good to me.\n",
            )
        );
        // Each dedicated header has exactly one space after the colon, the form
        // git requires to honor it rather than duplicate it.
        assert!(out.contains("\nIn-Reply-To: <"));
        assert!(out.contains("\nReferences: <"));
        // git owns MIME, Date, and Message-ID; Koshi emits none of them.
        assert!(!out.contains("MIME-Version"));
        assert!(!out.contains("Content-Transfer-Encoding"));
        assert!(!out.contains("Content-Type"));
        assert!(!out.contains("\nDate:"));
        assert!(!out.contains("Message-ID"));
    }

    #[test]
    fn render_keeps_a_non_ascii_subject_readable() {
        // The Subject is not RFC 2047-encoded here — git encodes it on send, so
        // the composer shows it as typed.
        let mut msg = sample();
        msg.subject = "Re: café".to_string();
        assert!(render(&msg).contains("\nSubject: Re: café\n"));
    }

    #[test]
    fn render_folds_a_multi_address_cc_one_per_line() {
        let mut msg = sample();
        msg.cc = "a@x, b@x, c@x".to_string();
        assert!(render(&msg).contains("Cc: a@x,\n    b@x,\n    c@x\n"));
        // A single address does not fold.
        msg.cc = "only@x".to_string();
        assert!(render(&msg).contains("Cc: only@x\n"));
    }

    #[test]
    fn fold_and_unfold_round_trip() {
        let folded = fold_addresses("a@x, b@x, c@x");
        assert_eq!(folded, "a@x,\n    b@x,\n    c@x");
        // Unfolding a folded list (or any continuation whitespace) recovers the
        // clean single-line comma list git wants.
        assert_eq!(unfold(&folded), "a@x, b@x, c@x");
        assert_eq!(unfold("<a>\n    <b>\n\t<c>"), "<a> <b> <c>");
        // A plain single-line value is unchanged.
        assert_eq!(unfold("Nika <nika@x>"), "Nika <nika@x>");
        assert_eq!(unfold(""), "");
    }

    #[test]
    fn render_omits_cc_and_threading_when_absent() {
        let mut msg = sample();
        msg.cc = String::new();
        msg.in_reply_to = String::new();
        msg.references = String::new();
        let out = render(&msg);
        assert!(!out.contains("\nCc:"));
        assert!(!out.contains("In-Reply-To:"));
        assert!(!out.contains("References:"));
    }

    #[test]
    fn render_advertises_the_user_agent_only_when_set() {
        let mut msg = sample();
        msg.user_agent = Some("koshi/1.2.3".to_string());
        assert!(render(&msg).contains("\nUser-Agent: koshi/1.2.3\n"));

        msg.user_agent = None;
        assert!(!render(&msg).contains("User-Agent"));
    }

    #[test]
    fn parse_headers_splits_headers_from_body() {
        let raw = "From: a@b\nTo: c@d\nSubject: hi\n\nBody line one.\nLine two.\n";
        let (headers, body) = parse_headers(raw);
        assert_eq!(header_value(&headers, "From"), Some("a@b"));
        assert_eq!(header_value(&headers, "to"), Some("c@d")); // case-insensitive
        assert_eq!(header_value(&headers, "Subject"), Some("hi"));
        assert_eq!(header_value(&headers, "Cc"), None);
        assert_eq!(body, "Body line one.\nLine two.\n");
    }

    #[test]
    fn parse_headers_folds_continuation_lines() {
        let raw = "References: <a@x>\n <b@x>\n\tbody-not\n\nBody.\n";
        let (headers, _) = parse_headers(raw);
        assert_eq!(
            header_value(&headers, "References"),
            Some("<a@x>\n <b@x>\n\tbody-not")
        );
    }

    #[test]
    fn parse_headers_treats_a_message_with_no_blank_line_as_all_headers() {
        let (headers, body) = parse_headers("From: a@b\nTo: c@d\n");
        assert_eq!(header_value(&headers, "To"), Some("c@d"));
        assert_eq!(body, "");
    }

    #[test]
    fn parse_headers_round_trips_render() {
        let out = render(&sample());
        let (headers, body) = parse_headers(&out);
        assert_eq!(
            header_value(&headers, "To"),
            Some("Maintainer <maint@kernel.org>")
        );
        assert_eq!(header_value(&headers, "Cc"), Some("list@vger.kernel.org"));
        assert_eq!(body, "Looks good to me.\n");
    }

    #[test]
    fn references_chain_appends_parent_and_avoids_a_duplicate_tail() {
        assert_eq!(
            build_references("<a@x> <b@x>", "<c@x>"),
            ["<a@x>", "<b@x>", "<c@x>"]
        );
        // Parent already ends the parent's own References (a self-reply quirk).
        assert_eq!(build_references("<a@x> <b@x>", "<b@x>"), ["<a@x>", "<b@x>"]);
        // No parent references: just the parent id.
        assert_eq!(build_references("", "<c@x>"), ["<c@x>"]);
        // No parent at all: nothing.
        assert!(build_references("", "").is_empty());
    }

    #[test]
    fn references_are_capped_keeping_root_and_recent() {
        let ids: Vec<String> = (0..30).map(|n| format!("id{n}")).collect();
        let capped = cap_references(&ids, 25);
        assert_eq!(capped.len(), 25);
        assert_eq!(capped[0], "id0");
        assert_eq!(capped.last().unwrap(), "id29");
    }

    #[test]
    fn reference_ids_extracts_every_bracketed_token() {
        assert_eq!(reference_ids("<a@x>\n <b@x>  <c@x>"), ["a@x", "b@x", "c@x"]);
        // Bracketless value falls back to whitespace split.
        assert_eq!(reference_ids("a@x b@x"), ["a@x", "b@x"]);
        assert!(reference_ids("   ").is_empty());
    }

    #[test]
    fn dedup_cc_drops_blanks_dupes_and_the_reply_target() {
        let cc = [
            "Maintainer <maint@kernel.org>".to_string(), // same as `to`
            "list@vger.kernel.org".to_string(),
            "  ".to_string(),
            "LIST@vger.kernel.org".to_string(), // case-insensitive dupe
            "other@x.org".to_string(),
        ];
        assert_eq!(
            dedup_cc("maint@kernel.org", &cc),
            ["list@vger.kernel.org", "other@x.org"]
        );
    }

    #[test]
    fn normalize_body_uses_lf_and_one_trailing_newline() {
        assert_eq!(normalize_body("a\r\nb\rc"), "a\nb\nc\n");
        assert_eq!(normalize_body("a\n\n\n"), "a\n");
        assert_eq!(normalize_body(""), "\n");
    }
}
