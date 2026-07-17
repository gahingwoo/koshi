//! A GUI password prompt for `git send-email`, bridged through `GIT_ASKPASS`.
//!
//! When git needs an SMTP password it runs the program named by `GIT_ASKPASS`
//! with the prompt as `argv[1]` and reads the secret from its stdout. Koshi
//! points `GIT_ASKPASS` at a tiny client that asks the main process — over a
//! private unix socket — for the password and prints it. That client is
//! normally the standalone `koshi-askpass` binary (which starts in ~3ms because
//! it loads none of the GUI libraries); when that binary is not installed beside
//! Koshi (e.g. a bare `cargo run`, which builds only one binary), Koshi falls
//! back to re-exec'ing *itself* — [`helper_prompt`] detects that before any GTK
//! setup and [`run_helper`] does the same exchange. Under Flatpak, where git
//! runs on the host and cannot execute a sandbox path, `GIT_ASKPASS` is instead
//! a host-side script that re-enters the sandbox as the helper (see
//! [`write_host_askpass`]). In every case Koshi sets `GIT_ASKPASS` itself, so
//! git never reaches an inherited, foreign askpass.
//!
//! Security properties (the password's whole path is GTK entry → socket →
//! helper stdout → git):
//!
//! - The password is never on any command line. The only thing on the helper's
//!   `argv` is git's prompt string — the *account*, not the secret, and no more
//!   than is already in the user's git config. `--smtp-pass` is never used.
//! - The password is never in the child's environment. The child gets only the
//!   socket path and a one-time token; `/proc/<pid>/environ` is owner-only
//!   anyway, and the token is belt-and-braces against a same-uid race.
//! - The socket lives in a `0700` directory under the XDG runtime dir (tmpfs),
//!   with a per-send random name, and is removed when the send finishes.
//! - Koshi stores the password nowhere. It caches it only in memory for the
//!   session (so one send does not prompt repeatedly), and git's own
//!   `credential approve` still populates any configured credential helper.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk::{gio, glib};

/// The name of the fast standalone helper binary (`src/bin/koshi-askpass.rs`),
/// preferred over the re-exec fallback because it loads none of Koshi's GUI
/// libraries.
const HELPER_BINARY: &str = "koshi-askpass";
/// Names the socket the helper connects to. Kept in step with the helper binary.
const SOCKET_ENV: &str = "KOSHI_ASKPASS_SOCKET";
/// The one-time token the helper must present, so only the send that spawned it
/// can drive the prompt.
const TOKEN_ENV: &str = "KOSHI_ASKPASS_TOKEN";

// ---- helper side (used when Koshi re-execs itself as the askpass fallback) ---

/// `Some(prompt)` when this process was re-exec'd as `GIT_ASKPASS` — i.e. the
/// askpass socket is set in the environment. Returns git's prompt (`argv[1]`).
/// Call it at the very top of `main`, before any GTK initialisation.
pub fn helper_prompt() -> Option<String> {
    std::env::var_os(SOCKET_ENV)?;
    Some(std::env::args().nth(1).unwrap_or_default())
}

/// Run the askpass exchange and print the password for git — the same job the
/// standalone `koshi-askpass` binary does, for the re-exec fallback path. Exit 0
/// with the secret, or non-zero (printing nothing) on cancel/error so git falls
/// back and, with `GIT_TERMINAL_PROMPT=0`, fails cleanly.
pub fn run_helper(prompt: &str) -> glib::ExitCode {
    let Some(password) = helper_exchange(prompt) else {
        return glib::ExitCode::FAILURE;
    };
    let mut stdout = io::stdout();
    let wrote = stdout
        .write_all(password.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush());
    if wrote.is_err() {
        return glib::ExitCode::FAILURE;
    }
    glib::ExitCode::SUCCESS
}

/// Connect to the socket, present the token and prompt, read back the password.
fn helper_exchange(prompt: &str) -> Option<String> {
    let socket = std::env::var_os(SOCKET_ENV)?;
    let token = std::env::var(TOKEN_ENV).ok()?;

    let mut stream = UnixStream::connect(socket).ok()?;
    write!(stream, "{token}\n{prompt}\n").ok()?;
    stream.flush().ok()?;

    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).ok()?;
    let encoded = reply.trim_end_matches('\n').strip_prefix("ok ")?;
    let bytes = decode_hex(encoded)?;
    String::from_utf8(bytes).ok()
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

// -------------------------------------------------------- session cache

thread_local! {
    /// Passwords entered this session, keyed by `smtp://user@host`. In memory
    /// only — never written to disk — and gone when Koshi exits.
    static CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

fn cache_get(key: &str) -> Option<String> {
    CACHE.with_borrow(|cache| cache.get(key).cloned())
}

fn cache_set(key: &str, password: &str) {
    CACHE.with_borrow_mut(|cache| {
        cache.insert(key.to_string(), password.to_string());
    });
}

/// Forget every session-cached password, so the next send prompts afresh. Backs
/// the "Forget SMTP password" action.
pub fn forget_all() {
    CACHE.with_borrow_mut(HashMap::clear);
}

// ---------------------------------------------------------------- server side

/// Counter making per-send socket directory names unique within a process.
static SERVER_COUNTER: AtomicU32 = AtomicU32::new(0);

/// The main-process end of the bridge for one send: a private directory holding
/// a symlink to this executable (the `GIT_ASKPASS` program) and a listening
/// unix socket. Dropped — directory removed, service stopped — when the send
/// finishes.
pub struct AskpassServer {
    dir: PathBuf,
    token: String,
    service: gio::SocketService,
}

impl AskpassServer {
    /// Set up the socket and the askpass symlink, serving password prompts from
    /// `parent`'s window until dropped. Fails if the helper binary is missing,
    /// so the caller can fall back to git's own (clean) failure.
    pub fn start(parent: &impl IsA<gtk::Widget>) -> io::Result<Self> {
        let parent: gtk::Widget = parent.clone().upcast();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let unique = format!(
            "send-{}-{}-{}",
            std::process::id(),
            now.as_nanos(),
            SERVER_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        // Under Flatpak git runs on the *host*, so the askpass program and
        // socket live in the runtime dir both sides see at the same path.
        let dir = crate::flatpak::shared_runtime_dir()
            .join("koshi")
            .join(unique);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;

        // git runs GIT_ASKPASS through a shell, which word-splits on the path;
        // a name under our own space keeps it free of shell metacharacters.
        let askpass = dir.join("askpass");
        match crate::flatpak::app_id() {
            Some(app_id) => write_host_askpass(&askpass, &app_id)?,
            None => std::os::unix::fs::symlink(askpass_program()?, &askpass)?,
        }

        let socket = dir.join("askpass.sock");
        let token = random_token()?;

        let service = gio::SocketService::new();
        let address = gio::UnixSocketAddress::new(&socket);
        service
            .add_address(
                &address,
                gio::SocketType::Stream,
                gio::SocketProtocol::Default,
                glib::Object::NONE,
            )
            .map_err(|error| io::Error::other(error.to_string()))?;

        service.connect_incoming(glib::clone!(
            #[strong]
            token,
            #[weak]
            parent,
            #[upgrade_or]
            true,
            move |_, connection, _| {
                glib::spawn_future_local(glib::clone!(
                    #[strong]
                    token,
                    #[weak]
                    parent,
                    #[strong]
                    connection,
                    async move {
                        handle_connection(&connection, &token, &parent).await;
                    }
                ));
                true
            }
        ));
        service.start();

        Ok(Self {
            dir,
            token,
            service,
        })
    }

    /// The environment git needs to reach this server: `GIT_ASKPASS` plus the
    /// socket path and one-time token the helper presents. Returned as data —
    /// not set on a launcher directly — because under Flatpak these must also
    /// travel to the host as explicit `--env=` flags.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "GIT_ASKPASS".to_string(),
                self.dir.join("askpass").to_string_lossy().into_owned(),
            ),
            (
                SOCKET_ENV.to_string(),
                self.dir.join("askpass.sock").to_string_lossy().into_owned(),
            ),
            (TOKEN_ENV.to_string(), self.token.clone()),
        ]
    }
}

/// Write the `GIT_ASKPASS` program for the Flatpak case: host git cannot
/// execute a path inside the sandbox, so the program is a host-side shell
/// script that re-enters the sandbox as a second instance of the app running
/// the helper binary. The socket and token are forwarded explicitly — git
/// carries `KOSHI_ASKPASS_*` in the environment it gives the script, and the
/// helper instance needs them back inside. The socket path itself is valid in
/// all three contexts because it lives in the shared per-app runtime dir.
fn write_host_askpass(path: &std::path::Path, app_id: &str) -> io::Result<()> {
    let script = format!(
        "#!/bin/sh\n\
         exec flatpak run \
         --env={SOCKET_ENV}=\"${SOCKET_ENV}\" \
         --env={TOKEN_ENV}=\"${TOKEN_ENV}\" \
         --command={HELPER_BINARY} {app_id} \"$@\"\n"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(path)?;
    file.write_all(script.as_bytes())
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.service.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Serve one prompt: verify the token, resolve the credential key, answer from
/// the session cache or a password dialog, and write the reply.
async fn handle_connection(connection: &gio::SocketConnection, token: &str, parent: &gtk::Widget) {
    let Some((got_token, prompt)) = read_request(connection).await else {
        write_reply(connection, "cancel").await;
        return;
    };
    // A mismatched or absent token means this is not our helper.
    if got_token != token {
        write_reply(connection, "cancel").await;
        return;
    }

    let key = credential_key(&prompt);
    let password = match cache_get(&key) {
        Some(cached) => Some(cached),
        None => {
            let entered = prompt_password(parent, &prompt).await;
            if let Some(password) = &entered {
                cache_set(&key, password);
            }
            entered
        }
    };

    match password {
        Some(password) => {
            let encoded = encode_hex(password.as_bytes());
            write_reply(connection, &format!("ok {encoded}")).await;
        }
        None => write_reply(connection, "cancel").await,
    }
}

/// Lowercase-hex encoding of the password for the reply line (see the helper's
/// `decode_hex`): keeps it on one newline-terminated line whatever bytes it
/// contains, with no dependency either side.
fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The program to install as `GIT_ASKPASS`: the fast standalone helper when it
/// is installed beside Koshi, otherwise Koshi itself (the re-exec fallback).
/// Either way Koshi provides the askpass, so git never uses an inherited one.
fn askpass_program() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if let Some(helper) = exe.parent().map(|dir| dir.join(HELPER_BINARY))
        && helper.exists()
    {
        return Ok(helper);
    }
    log::warn!(
        "{HELPER_BINARY} not installed beside koshi; using the slower \
         re-exec askpass fallback (run `cargo build` to build the fast helper)"
    );
    Ok(exe)
}

/// Read the helper's `token\nprompt\n` request, tolerating a split read.
async fn read_request(connection: &gio::SocketConnection) -> Option<(String, String)> {
    let input = connection.input_stream();
    let mut buffer = Vec::new();
    // The request is tiny; loop until two lines have arrived or the peer stops.
    loop {
        let bytes = input
            .read_bytes_future(4096, glib::Priority::DEFAULT)
            .await
            .ok()?;
        if bytes.is_empty() {
            break;
        }
        buffer.extend_from_slice(&bytes);
        if buffer.iter().filter(|&&byte| byte == b'\n').count() >= 2 {
            break;
        }
        // A prompt is well under a kilobyte; refuse to buffer unboundedly.
        if buffer.len() > 8192 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split('\n');
    let token = lines.next()?.to_string();
    let prompt = lines.next()?.to_string();
    Some((token, prompt))
}

async fn write_reply(connection: &gio::SocketConnection, reply: &str) {
    let mut line = reply.to_string();
    line.push('\n');
    let _ = connection
        .output_stream()
        .write_all_future(line.into_bytes(), glib::Priority::DEFAULT)
        .await;
}

/// Show the SMTP password dialog and return what the user entered, or `None` if
/// they cancelled.
async fn prompt_password(parent: &gtk::Widget, prompt: &str) -> Option<String> {
    let dialog = adw::AlertDialog::new(
        Some("SMTP Password"),
        Some(&format!(
            "git send-email needs a password to authenticate.\n\n{}",
            prompt.trim()
        )),
    );

    let entry = adw::PasswordEntryRow::builder()
        .title("Password")
        .activates_default(true)
        .build();
    // Give the content a comfortable width so the description does not wrap onto
    // extra lines and a normal-length password fits without scrolling — the
    // AlertDialog grows to fit its extra child.
    let group = adw::PreferencesGroup::builder().width_request(360).build();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));
    dialog.set_prefer_wide_layout(true);

    dialog.add_responses(&[("cancel", "Cancel"), ("authenticate", "Authenticate")]);
    dialog.set_response_appearance("authenticate", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("authenticate"));
    dialog.set_close_response("cancel");

    let response = dialog.choose_future(Some(parent)).await;
    if response == "authenticate" {
        Some(entry.text().to_string())
    } else {
        None
    }
}

/// The credential key a password is cached under: the `smtp://user@host` inside
/// git's `Password for '...':` prompt, or the whole prompt if it has no quoted
/// target.
fn credential_key(prompt: &str) -> String {
    match (prompt.find('\''), prompt.rfind('\'')) {
        (Some(start), Some(end)) if start < end => prompt[start + 1..end].to_string(),
        _ => prompt.trim().to_string(),
    }
}

/// 16 bytes of kernel randomness, hex-encoded.
fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_key_extracts_the_quoted_target() {
        assert_eq!(
            credential_key("Password for 'smtp://nika@smtp.purelymail.com:465': "),
            "smtp://nika@smtp.purelymail.com:465"
        );
        // No quotes: fall back to the trimmed prompt.
        assert_eq!(credential_key("  Password:  "), "Password:");
    }

    #[test]
    fn random_token_is_32_hex_chars_and_varies() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn session_cache_round_trips() {
        let key = "smtp://test@example.invalid";
        assert_eq!(cache_get(key), None);
        cache_set(key, "secret");
        assert_eq!(cache_get(key).as_deref(), Some("secret"));
    }
}
