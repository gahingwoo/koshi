//! Quote-depth and unified-diff highlighting for mail bodies and the
//! composer. The classifier is pure so it can be unit-tested; the GTK glue
//! applies its spans as text tags, which keeps us on stock Adwaita (colors
//! come from tag properties, not CSS).

use adw::prelude::*;
use gtk::{gdk, glib};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Quote(usize),
    DiffAdd,
    DiffRemove,
    DiffHunk,
    DiffHeader,
    DiffMeta,
}

/// One highlighted range: `start..end` are character offsets within `line`
/// (TextBuffer iters count characters, not bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// Diff state machine. Diff mode is only entered on hard evidence (a
/// "diff --git" header or a "--- "/"+++ " pair), so code-looking prose is
/// never mis-tagged; anything malformed drops back to `None`.
enum State {
    None,
    /// After a diff header, before the first hunk: meta lines are expected.
    Preamble,
    /// Inside a hunk with this many old/new lines still unaccounted for.
    Hunk {
        old: u64,
        new: u64,
    },
    /// Hunk counts exhausted: only a new hunk, a new header or a
    /// "\ No newline..." marker may continue the diff.
    AfterHunk,
}

/// What the state machine decided for one line's content.
enum Outcome {
    /// The line belongs to the diff; `Some` carries its tag, `None` means a
    /// context line (kept default-colored on purpose).
    InDiff(Option<Kind>),
    NotDiff,
}

const META_PREFIXES: [&str; 12] = [
    "index ",
    "old mode ",
    "new mode ",
    "new file mode ",
    "deleted file mode ",
    "similarity index ",
    "rename from ",
    "rename to ",
    "copy from ",
    "copy to ",
    "Binary files ",
    "GIT binary patch",
];

pub fn classify(text: &str) -> Vec<Span> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut spans = Vec::new();
    let mut state = State::None;
    let mut state_depth = 0usize;

    for (n, line) in lines.iter().enumerate() {
        let (depth, prefix) = split_quote(line);
        // Diff state never survives a quote-depth change: a diff quoted at
        // one depth cannot continue in text quoted at another.
        if depth != state_depth {
            state = State::None;
            state_depth = depth;
        }
        // Strip a trailing '\r' so CRLF bodies (mailparse emits them for
        // quoted-printable parts) classify the same as LF ones. Span offsets
        // still use the full line length; tagging the invisible '\r' is
        // harmless.
        let content = line[prefix..].strip_suffix('\r').unwrap_or(&line[prefix..]);
        let next_is_plus = lines.get(n + 1).is_some_and(|next| {
            let (d, p) = split_quote(next);
            d == depth && next[p..].starts_with("+++ ")
        });

        let (next_state, outcome) = step(state, content, next_is_plus, depth > 0);
        state = next_state;

        let line_chars = line.chars().count();
        match outcome {
            Outcome::InDiff(kind) => {
                push(
                    &mut spans,
                    n,
                    0,
                    prefix,
                    (depth > 0).then_some(Kind::Quote(depth)),
                );
                push(&mut spans, n, prefix, line_chars, kind);
            }
            Outcome::NotDiff => {
                let quote = (depth > 0).then_some(Kind::Quote(depth));
                push(&mut spans, n, 0, line_chars, quote);
            }
        }
    }
    spans
}

/// Per line of `text`, `true` if the line must be preserved verbatim by a
/// rewrap pass and `false` if it is plain prose safe to reflow. Structural
/// lines are quoted lines (any depth) and every part of a unified diff —
/// headers, hunks, added/removed and context lines — reusing the same state
/// machine [`classify`] drives. When a diff is present, the `---` scissors and
/// diffstat that git format-patch places just above it are protected too, so
/// the whole patch survives; that back-fill only runs when a real diff was
/// found, keeping it from firing on a prose `---` rule or an `a | b` table.
///
/// A bare `-- ` line (the RFC 3676 signature separator, which git
/// format-patch emits above its version footer) freezes everything from there
/// to the end, so a trailing signature or `-- \n2.55.0` footer is never
/// reflowed even when it is not the user's configured signature.
pub fn preserve_mask(text: &str) -> Vec<bool> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut mask = vec![false; lines.len()];
    let mut state = State::None;
    let mut state_depth = 0usize;
    let mut first_diff: Option<usize> = None;
    let mut in_signature = false;

    for (n, line) in lines.iter().enumerate() {
        let (depth, prefix) = split_quote(line);
        if depth != state_depth {
            state = State::None;
            state_depth = depth;
        }
        let content = line[prefix..].strip_suffix('\r').unwrap_or(&line[prefix..]);
        let next_is_plus = lines.get(n + 1).is_some_and(|next| {
            let (d, p) = split_quote(next);
            d == depth && next[p..].starts_with("+++ ")
        });
        let (next_state, outcome) = step(state, content, next_is_plus, depth > 0);
        state = next_state;

        let in_diff = matches!(outcome, Outcome::InDiff(_));
        if in_diff && first_diff.is_none() {
            first_diff = Some(n);
        }
        // An unquoted `-- ` sigdash starts a signature that runs to the end.
        if depth == 0 && content == "-- " {
            in_signature = true;
        }
        mask[n] = in_signature || depth > 0 || in_diff;
    }

    if let Some(first) = first_diff {
        // Walk back over the blank gap and the diffstat/scissors block above
        // the first diff line, stopping at the commit message prose.
        for n in (0..first).rev() {
            let line = lines[n].trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            if is_diffstat_line(line) {
                mask[n] = true;
            } else {
                break;
            }
        }
    }
    mask
}

/// Whether `line` looks like a git-format-patch diffstat entry or the `---`
/// scissors that separates the commit message from the diff. Only consulted
/// for the lines immediately above a confirmed diff, so the loose matching
/// here cannot mis-fire on unrelated prose.
fn is_diffstat_line(line: &str) -> bool {
    let trimmed = line.trim();
    // The `---` scissors git format-patch places between message and diff.
    if trimmed == "---" {
        return true;
    }
    // A per-file stat: " path/to/file | 3 +-" (or "| Bin ..." / "old => new |").
    if line.starts_with(' ') && trimmed.contains(" | ") {
        return true;
    }
    // The summary: " 2 files changed, 3 insertions(+), 1 deletion(-)".
    if trimmed.contains("changed") && trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    false
}

fn push(spans: &mut Vec<Span>, line: usize, start: usize, end: usize, kind: Option<Kind>) {
    if let Some(kind) = kind
        && start < end
    {
        spans.push(Span {
            line,
            start,
            end,
            kind,
        });
    }
}

fn step(state: State, content: &str, next_is_plus: bool, quoted: bool) -> (State, Outcome) {
    if content.starts_with("diff --git ") {
        return (State::Preamble, Outcome::InDiff(Some(Kind::DiffHeader)));
    }
    match state {
        State::None => {
            // Quoted excerpts often resume mid-diff (after "[ ... ]" elision
            // or with no per-excerpt header at all), so a valid hunk header
            // re-enters diff mode when quoted. Unquoted prose keeps the
            // stricter rules below so lookalikes stay untagged.
            if quoted && let Some((old, new)) = parse_hunk(content) {
                return (hunk_state(old, new), Outcome::InDiff(Some(Kind::DiffHunk)));
            }
            // Plain `diff -u` output has no git header; require the
            // "--- "/"+++ " pair before believing it is a diff.
            if content.starts_with("--- ") && next_is_plus {
                (State::Preamble, Outcome::InDiff(Some(Kind::DiffHeader)))
            } else {
                (State::None, Outcome::NotDiff)
            }
        }
        State::Preamble => {
            if let Some((old, new)) = parse_hunk(content) {
                (hunk_state(old, new), Outcome::InDiff(Some(Kind::DiffHunk)))
            } else if content.starts_with("--- ") || content.starts_with("+++ ") {
                (State::Preamble, Outcome::InDiff(Some(Kind::DiffHeader)))
            } else if META_PREFIXES.iter().any(|p| content.starts_with(p)) {
                (State::Preamble, Outcome::InDiff(Some(Kind::DiffMeta)))
            } else {
                (State::None, Outcome::NotDiff)
            }
        }
        State::Hunk { old, new } => {
            // Some mail systems strip the lone leading space from blank
            // context lines, so an empty content line counts as context.
            if content.starts_with('\\') {
                (
                    State::Hunk { old, new },
                    Outcome::InDiff(Some(Kind::DiffMeta)),
                )
            } else if (content.starts_with(' ') || content.is_empty()) && old > 0 && new > 0 {
                (hunk_state(old - 1, new - 1), Outcome::InDiff(None))
            } else if content.starts_with('-') && old > 0 {
                (
                    hunk_state(old - 1, new),
                    Outcome::InDiff(Some(Kind::DiffRemove)),
                )
            } else if content.starts_with('+') && new > 0 {
                (
                    hunk_state(old, new - 1),
                    Outcome::InDiff(Some(Kind::DiffAdd)),
                )
            } else {
                // Truncated or trimmed diff: stop here, keep what was tagged.
                (State::None, Outcome::NotDiff)
            }
        }
        State::AfterHunk => {
            if let Some((old, new)) = parse_hunk(content) {
                (hunk_state(old, new), Outcome::InDiff(Some(Kind::DiffHunk)))
            } else if content.starts_with("--- ") || content.starts_with("+++ ") {
                (State::Preamble, Outcome::InDiff(Some(Kind::DiffHeader)))
            } else if content.starts_with('\\') {
                (State::AfterHunk, Outcome::InDiff(Some(Kind::DiffMeta)))
            } else {
                (State::None, Outcome::NotDiff)
            }
        }
    }
}

fn hunk_state(old: u64, new: u64) -> State {
    if old == 0 && new == 0 {
        State::AfterHunk
    } else {
        State::Hunk { old, new }
    }
}

/// Split off the quote prefix: leading '>'s with optional single spaces
/// between them ("> > >" and ">>>" are both depth 3), plus one trailing
/// space. Returns (depth, prefix length); the prefix is all ASCII, so the
/// length is valid as both a char and a byte offset.
fn split_quote(line: &str) -> (usize, usize) {
    let bytes = line.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b'>' {
        depth += 1;
        i += 1;
        if i + 1 < bytes.len() && bytes[i] == b' ' && bytes[i + 1] == b'>' {
            i += 1;
        }
    }
    if depth > 0 && i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    (depth, i)
}

/// Parse `@@ -a[,b] +c[,d] @@...` and return the old/new line counts
/// (a missing count means 1). Anything may follow the trailing `@@`.
fn parse_hunk(content: &str) -> Option<(u64, u64)> {
    let rest = content.strip_prefix("@@ -")?;
    let (old, rest) = parse_range(rest)?;
    let rest = rest.strip_prefix(" +")?;
    let (new, rest) = parse_range(rest)?;
    rest.strip_prefix(" @@")?;
    Some((old, new))
}

fn parse_range(s: &str) -> Option<(u64, &str)> {
    let (_, rest) = take_number(s)?;
    match rest.strip_prefix(',') {
        Some(rest) => take_number(rest),
        None => Some((1, rest)),
    }
}

fn take_number(s: &str) -> Option<(u64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..end].parse().ok()?;
    Some((n, &s[end..]))
}

const QUOTE_TAG: &str = "koshi-quote";
const ADD_TAG: &str = "koshi-diff-add";
const REMOVE_TAG: &str = "koshi-diff-remove";
const HUNK_TAG: &str = "koshi-diff-hunk";
const HEADER_TAG: &str = "koshi-diff-header";
const META_TAG: &str = "koshi-diff-meta";
const ALL_TAGS: [&str; 6] = [
    QUOTE_TAG, ADD_TAG, REMOVE_TAG, HUNK_TAG, HEADER_TAG, META_TAG,
];

/// Find-in-thread highlight tags. Deliberately kept out of ALL_TAGS: the
/// quote/diff refresh must not strip them, and they paint a background (not a
/// foreground), so a match keeps its line's quote/diff coloring underneath.
/// The current-match tag is created after the plain match tag so it wins where
/// they overlap (tag priority defaults to creation order).
const SEARCH_TAG: &str = "koshi-search";
const SEARCH_CURRENT_TAG: &str = "koshi-search-current";

/// Composer-only trailing-whitespace tag. Like the search tags it stays out of
/// ALL_TAGS and paints a background, so the quote/diff refresh never strips it
/// and the underlying text coloring shows through. Only the editable composer
/// applies it (via [`mark_trailing_whitespace`]); read-only bodies create the
/// tag but never use it, so received mail is not littered with the flag.
const TRAILING_TAG: &str = "koshi-trailing-ws";

/// GNOME palette colors per scheme: quote, then add, remove, hunk, header,
/// meta.
struct Palette {
    quote: &'static str,
    add: &'static str,
    remove: &'static str,
    hunk: &'static str,
    header: &'static str,
    meta: &'static str,
    /// Backgrounds for the search match and the current search match.
    search: &'static str,
    search_current: &'static str,
    /// Background flagging trailing whitespace in the composer.
    trailing: &'static str,
}

const LIGHT: Palette = Palette {
    quote: "#1a5fb4",
    add: "#26a269",
    remove: "#c01c28",
    hunk: "#1a5fb4",
    header: "#813d9c",
    meta: "#5e5c64",
    search: "#f9f06b",
    search_current: "#ffbe6f",
    trailing: "#f66151",
};

const DARK: Palette = Palette {
    quote: "#62a0ea",
    add: "#57e389",
    remove: "#ed333b",
    hunk: "#62a0ea",
    header: "#c061cb",
    meta: "#9a9996",
    search: "#665c00",
    search_current: "#a15d00",
    trailing: "#c01c28",
};

fn tag_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Quote(_) => QUOTE_TAG,
        Kind::DiffAdd => ADD_TAG,
        Kind::DiffRemove => REMOVE_TAG,
        Kind::DiffHunk => HUNK_TAG,
        Kind::DiffHeader => HEADER_TAG,
        Kind::DiffMeta => META_TAG,
    }
}

/// Create this module's tags in the buffer and keep their colors in sync
/// with the color scheme. The quote tag is created first so the diff tags,
/// created later, win the foreground where a diff span overlaps a quote
/// prefix (tag priority defaults to creation order).
pub fn attach(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();
    if table.lookup(QUOTE_TAG).is_some() {
        return;
    }
    for name in [QUOTE_TAG, ADD_TAG, REMOVE_TAG, META_TAG] {
        buffer.create_tag(Some(name), &[]);
    }
    for name in [HUNK_TAG, HEADER_TAG] {
        buffer.create_tag(Some(name), &[("weight", &700i32)]);
    }
    for name in [SEARCH_TAG, SEARCH_CURRENT_TAG, TRAILING_TAG] {
        buffer.create_tag(Some(name), &[]);
    }

    let style = adw::StyleManager::default();
    apply_colors(buffer, style.is_dark());
    let handler = style.connect_dark_notify(glib::clone!(
        #[weak]
        buffer,
        move |style| apply_colors(&buffer, style.is_dark())
    ));
    // Disconnect when the buffer goes away so long sessions do not pile up
    // dead handlers on the process-wide StyleManager. All GTK code runs on
    // the main thread, so the local variant is fine.
    let key = buffer.as_ptr() as usize;
    buffer.add_weak_ref_notify_local(move || {
        adw::StyleManager::default().disconnect(handler);
        SPAN_CACHE.with_borrow_mut(|cache| {
            cache.remove(&key);
        });
        PAINT_PROGRESS.with_borrow_mut(|progress| {
            progress.remove(&key);
        });
    });
}

fn apply_colors(buffer: &gtk::TextBuffer, dark: bool) {
    let palette = if dark { &DARK } else { &LIGHT };
    let colors = [
        (QUOTE_TAG, palette.quote),
        (ADD_TAG, palette.add),
        (REMOVE_TAG, palette.remove),
        (HUNK_TAG, palette.hunk),
        (HEADER_TAG, palette.header),
        (META_TAG, palette.meta),
    ];
    let table = buffer.tag_table();
    for (name, hex) in colors {
        if let Some(tag) = table.lookup(name) {
            let rgba = gdk::RGBA::parse(hex).expect("palette hex is valid");
            tag.set_property("foreground-rgba", rgba);
        }
    }
    // Search and trailing-whitespace tags carry a background rather than a
    // foreground.
    for (name, hex) in [
        (SEARCH_TAG, palette.search),
        (SEARCH_CURRENT_TAG, palette.search_current),
        (TRAILING_TAG, palette.trailing),
    ] {
        if let Some(tag) = table.lookup(name) {
            let rgba = gdk::RGBA::parse(hex).expect("palette hex is valid");
            tag.set_property("background-rgba", rgba);
        }
    }
}

/// Paint find-in-thread matches over a body buffer: every `ranges` entry gets
/// the match background, and the entry at `current` (if any) the stronger
/// current-match background on top. Any previous search highlight is cleared
/// first. Offsets are character offsets, the same addressing classify uses.
pub fn mark_search(buffer: &gtk::TextBuffer, ranges: &[(i32, i32)], current: Option<usize>) {
    clear_search(buffer);
    let table = buffer.tag_table();
    let (Some(match_tag), Some(current_tag)) =
        (table.lookup(SEARCH_TAG), table.lookup(SEARCH_CURRENT_TAG))
    else {
        return;
    };
    for &(start, end) in ranges {
        let from = buffer.iter_at_offset(start);
        let to = buffer.iter_at_offset(end);
        buffer.apply_tag(&match_tag, &from, &to);
    }
    if let Some(idx) = current
        && let Some(&(start, end)) = ranges.get(idx)
    {
        let from = buffer.iter_at_offset(start);
        let to = buffer.iter_at_offset(end);
        buffer.apply_tag(&current_tag, &from, &to);
    }
}

/// Drop any find-in-thread highlight from a body buffer.
pub fn clear_search(buffer: &gtk::TextBuffer) {
    let (start, end) = buffer.bounds();
    let table = buffer.tag_table();
    for name in [SEARCH_TAG, SEARCH_CURRENT_TAG] {
        if let Some(tag) = table.lookup(name) {
            buffer.remove_tag(&tag, &start, &end);
        }
    }
}

/// The trailing whitespace in `text`, as absolute character-offset ranges
/// `start..end` covering the run of spaces/tabs at the end of each line.
///
/// Every stray run is flagged, with no exceptions — including the signature
/// separator line `"-- "`: its trailing space is easy to lose track of, so it
/// gets the same visible flag as any other. A line that is nothing but
/// whitespace flags in full. Offsets are character offsets, the addressing the
/// buffer glue uses.
pub fn trailing_whitespace(text: &str) -> Vec<(i32, i32)> {
    let mut ranges = Vec::new();
    let mut line_start = 0i32;
    for line in text.split('\n') {
        let chars = line.chars().count() as i32;
        let kept = line.trim_end_matches([' ', '\t']).chars().count() as i32;
        if kept < chars {
            ranges.push((line_start + kept, line_start + chars));
        }
        // Advance past this line and the '\n' that split() consumed.
        line_start += chars + 1;
    }
    ranges
}

/// Flag trailing whitespace in an editable composer buffer: clear the previous
/// flags, then paint the redish background over each stray run. Applying and
/// removing tags emits no "changed", so this is safe to call from a changed
/// handler alongside [`refresh`]. Read-only bodies never call this.
pub fn mark_trailing_whitespace(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();
    let Some(tag) = table.lookup(TRAILING_TAG) else {
        return;
    };
    let (start, end) = buffer.bounds();
    buffer.remove_tag(&tag, &start, &end);
    let text = buffer.text(&start, &end, true);
    for (from, to) in trailing_whitespace(&text) {
        let a = buffer.iter_at_offset(from);
        let b = buffer.iter_at_offset(to);
        buffer.apply_tag(&tag, &a, &b);
    }
}

thread_local! {
    /// Last spans applied per buffer, keyed by pointer. Entries are removed
    /// by the weak-ref notify registered in `attach`, so the map only holds
    /// live buffers.
    static SPAN_CACHE: std::cell::RefCell<std::collections::HashMap<usize, Vec<Span>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Re-run classification over the whole buffer and retag only the lines
/// whose spans changed since the last refresh, so a keystroke touches O(1)
/// lines instead of the whole buffer. Applying tags does not emit
/// "changed", so this is safe to call from a changed handler.
///
/// Addressing uses absolute character offsets computed from the same text
/// classify saw: `iter_at_offset` has no line-ending semantics, so lines
/// GtkTextBuffer would split at lone '\r' or U+2029 (which classify does
/// not) cannot skew the mapping.
pub fn refresh(buffer: &gtk::TextBuffer) {
    let (start, end) = buffer.bounds();
    let text = buffer.text(&start, &end, true);
    let mut line_starts = vec![0i32];
    let mut off = 0i32;
    for ch in text.chars() {
        off += 1;
        if ch == '\n' {
            line_starts.push(off);
        }
    }
    let total_chars = off;

    let spans = classify(&text);
    let key = buffer.as_ptr() as usize;
    let old = SPAN_CACHE
        .with_borrow(|cache| cache.get(&key).cloned())
        .unwrap_or_default();

    let by_line = |spans: &[Span], lines: usize| {
        let mut per: Vec<Vec<Span>> = vec![Vec::new(); lines];
        for span in spans {
            if span.line < lines {
                per[span.line].push(*span);
            }
        }
        per
    };
    let new_lines = by_line(&spans, line_starts.len());
    let old_lines = by_line(&old, line_starts.len());

    let table = buffer.tag_table();
    for (n, (new, old)) in new_lines.iter().zip(&old_lines).enumerate() {
        if new == old {
            continue;
        }
        let base = line_starts[n];
        let line_end = line_starts.get(n + 1).copied().unwrap_or(total_chars);
        let from = buffer.iter_at_offset(base);
        let to = buffer.iter_at_offset(line_end);
        for name in ALL_TAGS {
            if let Some(tag) = table.lookup(name) {
                buffer.remove_tag(&tag, &from, &to);
            }
        }
        for span in new {
            let from = buffer.iter_at_offset(base + span.start as i32);
            let to = buffer.iter_at_offset(base + span.end as i32);
            buffer.apply_tag_by_name(tag_name(span.kind), &from, &to);
        }
    }
    SPAN_CACHE.with_borrow_mut(|cache| {
        cache.insert(key, spans);
    });
}

/// A partially applied highlight pass, so multi-megabyte bodies can be
/// colorized a slice at a time instead of freezing one frame for the whole
/// message (applying the tags dominates; a giant patch carries a span on
/// nearly every line).
struct PaintProgress {
    spans: Vec<Span>,
    line_starts: Vec<i32>,
    total_chars: i32,
    next_line: usize,
    next_span: usize,
}

thread_local! {
    static PAINT_PROGRESS: std::cell::RefCell<std::collections::HashMap<usize, PaintProgress>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Incremental refresh for a freshly filled, read-only buffer: classify the
/// whole text on the first call, then apply at most `lines` more lines of
/// tags per call. Returns true while further calls are needed. If the buffer
/// text changes between calls the pass starts over, so this must not be
/// mixed with editable buffers — use refresh there.
pub fn refresh_step(buffer: &gtk::TextBuffer, lines: usize) -> bool {
    let key = buffer.as_ptr() as usize;
    let stored = PAINT_PROGRESS.with_borrow_mut(|progress| progress.remove(&key));
    // A stale pass (buffer text replaced under it) restarts from scratch;
    // character count is a good-enough fingerprint for set_text swaps.
    let mut progress = match stored {
        Some(progress) if progress.total_chars == buffer.char_count() => progress,
        _ => {
            let (start, end) = buffer.bounds();
            let text = buffer.text(&start, &end, true);
            let mut line_starts = vec![0i32];
            let mut off = 0i32;
            for ch in text.chars() {
                off += 1;
                if ch == '\n' {
                    line_starts.push(off);
                }
            }
            PaintProgress {
                spans: classify(&text),
                line_starts,
                total_chars: off,
                next_line: 0,
                next_span: 0,
            }
        }
    };

    // The buffer is freshly set (set_text drops all tags), so tags are only
    // applied, never removed; spans arrive in line order from classify.
    let end_line = progress.next_line.saturating_add(lines);
    while progress.next_span < progress.spans.len() {
        let span = progress.spans[progress.next_span];
        if span.line >= end_line {
            break;
        }
        let base = progress.line_starts.get(span.line).copied().unwrap_or(0);
        let from = buffer.iter_at_offset(base + span.start as i32);
        let to = buffer.iter_at_offset(base + span.end as i32);
        buffer.apply_tag_by_name(tag_name(span.kind), &from, &to);
        progress.next_span += 1;
    }
    progress.next_line = end_line;

    if progress.next_line >= progress.line_starts.len() {
        // Done: leave the final spans where refresh's diffing expects them,
        // so a later full refresh sees the true tag state.
        SPAN_CACHE.with_borrow_mut(|cache| {
            cache.insert(key, progress.spans);
        });
        false
    } else {
        PAINT_PROGRESS.with_borrow_mut(|map| {
            map.insert(key, progress);
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds_by_line(text: &str) -> Vec<Vec<Kind>> {
        let mut lines = vec![Vec::new(); text.split('\n').count()];
        for span in classify(text) {
            lines[span.line].push(span.kind);
        }
        lines
    }

    #[test]
    fn preserve_mask_marks_prose_reflowable() {
        let mask = preserve_mask("first line\nsecond line\n\nthird");
        assert_eq!(mask, vec![false, false, false, false]);
    }

    #[test]
    fn preserve_mask_protects_quotes() {
        let mask = preserve_mask("reply\n> quoted\n>> deeper\nmore reply");
        assert_eq!(mask, vec![false, true, true, false]);
    }

    #[test]
    fn preserve_mask_protects_the_whole_patch() {
        let body = "\
Fix the frobnicator.

---
 src/frob.c | 3 ++-
 1 file changed, 2 insertions(+), 1 deletion(-)

diff --git a/src/frob.c b/src/frob.c
index 1111111..2222222 100644
--- a/src/frob.c
+++ b/src/frob.c
@@ -1,2 +1,2 @@
 keep
-old line
+new line";
        let mask = preserve_mask(body);
        // The commit message and the two blank gaps stay reflowable.
        assert!(!mask[0], "commit message frozen");
        assert!(!mask[1], "blank after message frozen");
        assert!(!mask[5], "blank before diff frozen");
        // The scissors, diffstat, and every diff line are preserved.
        for (n, line) in body.split('\n').enumerate() {
            if n == 0 || line.trim().is_empty() {
                continue;
            }
            assert!(mask[n], "line {n} not preserved: {line:?}");
        }
    }

    #[test]
    fn preserve_mask_leaves_a_lone_rule_alone_without_a_diff() {
        // A prose "---" with no diff after it is not a scissors, so it is not
        // force-frozen (it reflows to itself harmlessly either way).
        let mask = preserve_mask("intro\n\n---\n\nmore prose");
        assert_eq!(mask, vec![false, false, false, false, false]);
    }

    #[test]
    fn preserve_mask_freezes_after_a_sigdash() {
        // Everything from a bare "-- " to the end is a signature/footer.
        let mask = preserve_mask("prose line\n\n-- \n2.55.0");
        assert_eq!(mask, vec![false, false, true, true]);
    }

    #[test]
    fn git_patch_body_is_classified_and_wrapper_lines_are_not() {
        let body = "\
Fix the frobnicator.

---
 src/frob.c | 3 ++-
 1 file changed, 2 insertions(+), 1 deletion(-)

diff --git a/src/frob.c b/src/frob.c
index 1111111..2222222 100644
--- a/src/frob.c
+++ b/src/frob.c
@@ -1,2 +1,2 @@
 keep
-old line
+new line
@@ -10,2 +10,3 @@ int frob(void)
 keep
+added
 keep

--
nika";
        let lines = kinds_by_line(body);
        assert!(lines[0].is_empty());
        assert!(lines[2].is_empty(), "git-email --- separator tagged");
        assert!(
            lines[3].is_empty() && lines[4].is_empty(),
            "diffstat tagged"
        );
        assert_eq!(lines[6], [Kind::DiffHeader]);
        assert_eq!(lines[7], [Kind::DiffMeta]);
        assert_eq!(lines[8], [Kind::DiffHeader]);
        assert_eq!(lines[9], [Kind::DiffHeader]);
        assert_eq!(lines[10], [Kind::DiffHunk]);
        assert!(lines[11].is_empty(), "context line tagged");
        assert_eq!(lines[12], [Kind::DiffRemove]);
        assert_eq!(lines[13], [Kind::DiffAdd]);
        assert_eq!(lines[14], [Kind::DiffHunk]);
        assert_eq!(lines[16], [Kind::DiffAdd]);
        assert!(lines[19].is_empty(), "signature marker tagged");
        assert!(lines[20].is_empty());
    }

    #[test]
    fn diff_lookalikes_outside_diff_mode_are_untagged() {
        let body = "\
- item one
+ emphasis, mine
@@ weird @@
@@ -1,2 +1,2 @@
a --> b";
        for (n, kinds) in kinds_by_line(body).iter().enumerate() {
            assert!(kinds.is_empty(), "line {n} tagged: {kinds:?}");
        }
    }

    #[test]
    fn quote_depth_counts_gt_with_optional_spaces() {
        let lines = kinds_by_line("> x\n>> x\n> > > x\n>>> x");
        assert_eq!(lines[0], [Kind::Quote(1)]);
        assert_eq!(lines[1], [Kind::Quote(2)]);
        assert_eq!(lines[2], [Kind::Quote(3)]);
        assert_eq!(lines[3], [Kind::Quote(3)]);
    }

    #[test]
    fn quote_span_covers_whole_line_outside_diffs() {
        let spans = classify("> hello there");
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (0, 13));
    }

    #[test]
    fn quoted_diff_splits_prefix_and_payload() {
        let body = "\
> diff --git a/f b/f
> --- a/f
> +++ b/f
> @@ -1,2 +1,2 @@
>  keep
> -old
> +added
>> +not a diff line";
        let spans: Vec<Span> = classify(body).into_iter().filter(|s| s.line == 6).collect();
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].kind, spans[0].start, spans[0].end),
            (Kind::Quote(1), 0, 2)
        );
        assert_eq!(
            (spans[1].kind, spans[1].start, spans[1].end),
            (Kind::DiffAdd, 2, 8)
        );

        // Depth change resets diff state: the depth-2 line is quote-only.
        let lines = kinds_by_line(body);
        assert_eq!(lines[7], [Kind::Quote(2)]);
    }

    #[test]
    fn truncated_quoted_hunk_stops_without_tagging_the_reply() {
        let body = "\
> diff --git a/f b/f
> --- a/f
> +++ b/f
> @@ -1,5 +1,5 @@
>  keep
> -old
This part looks wrong to me.";
        let lines = kinds_by_line(body);
        assert_eq!(lines[5], [Kind::Quote(1), Kind::DiffRemove]);
        assert!(lines[6].is_empty(), "reply line tagged: {:?}", lines[6]);
    }

    #[test]
    fn minus_plus_pair_enters_diff_mode_without_git_header() {
        let body = "\
--- a/f
+++ b/f
@@ -1 +1 @@
-old
+new";
        let lines = kinds_by_line(body);
        assert_eq!(lines[0], [Kind::DiffHeader]);
        assert_eq!(lines[1], [Kind::DiffHeader]);
        assert_eq!(lines[2], [Kind::DiffHunk]);
        assert_eq!(lines[3], [Kind::DiffRemove]);
        assert_eq!(lines[4], [Kind::DiffAdd]);
    }

    #[test]
    fn lone_minus_minus_minus_line_does_not_enter_diff_mode() {
        let lines = kinds_by_line("--- a/f\nnot a plus line\n@@ -1 +1 @@");
        for (n, kinds) in lines.iter().enumerate() {
            assert!(kinds.is_empty(), "line {n} tagged: {kinds:?}");
        }
    }

    #[test]
    fn quoted_hunk_resumes_after_unquoted_elision_line() {
        let body = "\
> diff --git a/f b/f
> --- a/f
> +++ b/f
[ ... ]
> @@ -81,7 +81,7 @@ static void init(void)
>  keep
> -old
> +new";
        let lines = kinds_by_line(body);
        assert!(lines[3].is_empty(), "elision line tagged: {:?}", lines[3]);
        assert_eq!(lines[4], [Kind::Quote(1), Kind::DiffHunk]);
        assert_eq!(lines[5], [Kind::Quote(1)]);
        assert_eq!(lines[6], [Kind::Quote(1), Kind::DiffRemove]);
        assert_eq!(lines[7], [Kind::Quote(1), Kind::DiffAdd]);
    }

    #[test]
    fn quoted_excerpt_starting_at_hunk_header_enters_diff_mode() {
        let body = "\
Comment first.
> @@ -1,2 +1,2 @@
> -old
> +new";
        let lines = kinds_by_line(body);
        assert!(lines[0].is_empty());
        assert_eq!(lines[1], [Kind::Quote(1), Kind::DiffHunk]);
        assert_eq!(lines[2], [Kind::Quote(1), Kind::DiffRemove]);
        assert_eq!(lines[3], [Kind::Quote(1), Kind::DiffAdd]);
    }

    #[test]
    fn blank_context_line_with_stripped_space_stays_in_hunk() {
        let body = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
-old

+new";
        let lines = kinds_by_line(body);
        assert!(lines[5].is_empty());
        assert_eq!(lines[6], [Kind::DiffAdd]);
    }

    #[test]
    fn trailing_whitespace_flags_every_stray_run() {
        // "a" clean; "b  " has two trailing spaces; the sig separator "-- " is
        // flagged like anything else; "\t" tab-only line flags in full; the
        // closing "c" is clean.
        let text = "a\nb  \n-- \n\t\nc";
        // "a\n" = offsets 0,1; "b  \n" starts at 2, its "  " span is 3..5.
        // "-- \n" starts at 6, its trailing space is 8..9; "\t\n" starts at 10,
        // span 10..11.
        assert_eq!(trailing_whitespace(text), [(3, 5), (8, 9), (10, 11)]);
    }

    #[test]
    fn trailing_whitespace_leaves_clean_lines_alone() {
        assert!(trailing_whitespace("clean line").is_empty());
        assert!(trailing_whitespace("a\nb\nc").is_empty());
    }

    #[test]
    fn crlf_body_classifies_like_lf() {
        let body =
            "diff --git a/f b/f\r\n--- a/f\r\n+++ b/f\r\n@@ -1,3 +1,3 @@\r\n-old\r\n\r\n+new\r\n";
        let lines = kinds_by_line(body);
        assert_eq!(lines[0], [Kind::DiffHeader]);
        assert_eq!(lines[3], [Kind::DiffHunk]);
        assert_eq!(lines[4], [Kind::DiffRemove]);
        assert!(
            lines[5].is_empty(),
            "stripped blank context line tagged: {:?}",
            lines[5]
        );
        assert_eq!(lines[6], [Kind::DiffAdd]);
    }
}
