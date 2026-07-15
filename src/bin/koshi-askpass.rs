//! The `GIT_ASKPASS` helper for Koshi's send flow — a tiny, standalone program
//! that asks the running Koshi process for an SMTP password and prints it for
//! git.
//!
//! This is deliberately its *own* binary rather than a re-exec of Koshi: it
//! links only the standard library, so it starts in a couple of milliseconds
//! instead of loading Koshi's ~130 GUI libraries. Koshi points `GIT_ASKPASS` at
//! it; it connects to the per-send socket, presents the one-time token and
//! git's prompt, and reads back the password — which Koshi's own dialog
//! collected. The password never touches git's command line or environment.
//!
//! The socket protocol and env-var names must stay in step with `src/askpass.rs`
//! (the server half, in the main process).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

/// Names the socket to connect to.
const SOCKET_ENV: &str = "KOSHI_ASKPASS_SOCKET";
/// The one-time token proving this helper belongs to the send that spawned it.
const TOKEN_ENV: &str = "KOSHI_ASKPASS_TOKEN";

fn main() -> ExitCode {
    let Some(password) = ask() else {
        // No password (cancelled, or the bridge is unreachable): print nothing
        // and fail, so git falls back — and with GIT_TERMINAL_PROMPT=0 that is a
        // clean error rather than a hang.
        return ExitCode::FAILURE;
    };
    let mut stdout = std::io::stdout();
    let wrote = stdout
        .write_all(password.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush());
    if wrote.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Exchange with the main process: send `token\nprompt\n`, read back the
/// password. `None` on cancel or any I/O error.
fn ask() -> Option<String> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let token = std::env::var(TOKEN_ENV).ok()?;
    // git invokes an askpass program with the prompt as argv[1].
    let prompt = std::env::args().nth(1).unwrap_or_default();

    let mut stream = UnixStream::connect(socket).ok()?;
    write!(stream, "{token}\n{prompt}\n").ok()?;
    stream.flush().ok()?;

    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).ok()?;
    let encoded = reply.trim_end_matches('\n').strip_prefix("ok ")?;
    let bytes = decode_hex(encoded)?;
    String::from_utf8(bytes).ok()
}

/// Decode the lowercase-hex password the server sends (hex keeps the reply on a
/// single newline-terminated line whatever bytes the password contains).
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}
