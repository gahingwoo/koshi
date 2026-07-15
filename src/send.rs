//! Sending a reply through `git send-email`.
//!
//! Koshi shells out to the user's own `git send-email` rather than speaking
//! SMTP, so their existing transport config and credential helpers keep
//! working and Koshi never stores a password. The command is made a
//! deterministic, non-interactive, WYSIWYG transport:
//!
//! - Every recipient is passed explicitly (`--to`/`--cc`) and all *derived* Cc
//!   is suppressed (`--suppress-cc=all`, `--to-cmd=true`, `--no-header-cmd`),
//!   so the message reaches exactly who the composer shows — a body trailer or
//!   a `sendemail.to`/`ccCmd` in config cannot silently add recipients.
//! - Prompts are neutralised: `GIT_SEND_EMAIL_NOTTY=1` forces git's reader onto
//!   stdin (not `/dev/tty`, which redirection alone does not cover),
//!   `--confirm=never`/`--no-annotate`/`GIT_EDITOR=true` remove the interactive
//!   stops, and `GIT_TERMINAL_PROMPT=0` turns a needed-but-unavailable password
//!   into a fast, clean error instead of a hang. (The GUI password prompt is a
//!   later commit; until then a passwordless config simply fails cleanly.)
//! - The message is written to a private file in a scratch directory *outside*
//!   any git repository, which becomes the child's cwd, so a repo Koshi was
//!   launched in cannot run its `sendemail-validate` hook or leak its config.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gtk::prelude::*;
use gtk::{gio, glib};

/// Everything needed to send one reply.
pub struct Request {
    /// The `From:` — also passed as `--from` so it matches the message and git
    /// does not rewrite it.
    pub from: String,
    /// The `To:` recipients, as the composer's comma-separated field. git
    /// splits the list itself.
    pub to: String,
    /// The `Cc:` recipients, comma-separated; empty for none.
    pub cc: String,
    /// The fully rendered RFC 5322 message.
    pub eml: String,
}

/// The result of a send attempt.
pub enum Outcome {
    /// git send-email exited 0.
    Sent,
    /// git send-email failed; `stderr` is its diagnostic output for the user.
    Failed { code: Option<i32>, stderr: String },
}

/// Send `req` via `git send-email`, awaiting completion on the GLib main loop.
/// A GUI password prompt is served from `parent`'s window if git asks for one.
/// The scratch message file is removed before returning, whatever the outcome.
pub async fn send(req: Request, parent: &impl IsA<gtk::Widget>) -> io::Result<Outcome> {
    let scratch = Scratch::create(&req.eml)?;
    let argv = build_argv(&req.from, &req.to, &req.cc, &scratch.eml_path());

    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    );
    // Force git's terminal reader onto stdin so a stray prompt hits EOF and
    // takes its default rather than blocking on /dev/tty.
    launcher.setenv("GIT_SEND_EMAIL_NOTTY", "1", true);
    // A needed password with no helper becomes a clean error, not a hang.
    launcher.setenv("GIT_TERMINAL_PROMPT", "0", true);
    // Backstops against anything trying to open an editor.
    launcher.setenv("GIT_EDITOR", "true", true);
    // Stable, parseable diagnostics regardless of the user's locale.
    launcher.setenv("LC_ALL", "C", true);
    // A non-repo cwd: no repo-local config, no sendemail-validate hook.
    launcher.set_cwd(scratch.dir());
    launcher.set_stdin_file_path(Some("/dev/null"));

    // A GUI password prompt when git needs one, best-effort: if the bridge
    // can't be set up, git simply falls back and (with GIT_TERMINAL_PROMPT=0)
    // fails cleanly rather than hanging. Kept alive until the child exits.
    let askpass = crate::askpass::AskpassServer::start(parent);
    match &askpass {
        Ok(server) => server.install(&launcher),
        Err(error) => eprintln!("koshi: SMTP password prompt unavailable: {error}"),
    }

    let osargv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let subprocess = launcher.spawn(&osargv).map_err(to_io)?;
    let (stdout, stderr) = subprocess
        .communicate_utf8_future(None)
        .await
        .map_err(to_io)?;
    let stdout = stdout.map(|s| s.to_string()).unwrap_or_default();
    let stderr = stderr.map(|s| s.to_string()).unwrap_or_default();

    // git send-email's exit code is NOT trustworthy: it exits 0 even when a
    // message was not sent — e.g. when SMTP auth fails after the password prompt
    // is cancelled. So confirm success by git's own per-message acceptance line,
    // never by the exit code, or a failed send would be reported as sent (and
    // the composer would discard the draft).
    if sent_ok(&stdout) {
        Ok(Outcome::Sent)
    } else {
        Ok(Outcome::Failed {
            code: subprocess.has_exited().then(|| subprocess.exit_status()),
            stderr,
        })
    }
}

/// Whether git send-email confirmed it actually delivered the message. git
/// prints `OK. Log says:` once per message, and only after line 1857 of its
/// source has verified the server (or sendmail) accepted it — so this line,
/// not the exit code, is the reliable "sent" signal. `LC_ALL=C` keeps the
/// string stable regardless of the user's locale.
fn sent_ok(stdout: &str) -> bool {
    stdout.contains("OK. Log says:")
}

/// The full `git send-email` command line for `req`. Split out and pure so the
/// flag set — the load-bearing part of the transport — is unit-testable.
fn build_argv(from: &str, to: &str, cc: &str, eml: &Path) -> Vec<String> {
    let mut argv: Vec<String> = [
        "git",
        "send-email",
        // No interactive stops.
        "--confirm=never",
        "--no-annotate",
        // Single message: no chaining, and the file's own In-Reply-To/
        // References drive threading.
        "--no-thread",
        "--no-chain-reply-to",
        // Koshi guarantees a well-formed message; skip the hook and the
        // built-in line-length check, honoring "wrapping is manual".
        "--no-validate",
        // No X-Mailer fingerprint.
        "--no-xmailer",
        // The file is a message, never a revision list.
        "--no-format-patch",
        // Recipients come only from the flags below: suppress every derived Cc
        // and neutralise config-driven recipient commands.
        "--no-header-cmd",
        "--to-cmd=true",
        "--suppress-cc=all",
        // git owns the transfer encoding (its `auto` default: 8bit, or
        // quoted-printable when a line is too long) and adds the charset here.
        "--8bit-encoding=UTF-8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let from = from.trim();
    if !from.is_empty() {
        argv.push(format!("--from={from}"));
    }
    let to = to.trim();
    if !to.is_empty() {
        argv.push(format!("--to={to}"));
    }
    let cc = cc.trim();
    if !cc.is_empty() {
        argv.push(format!("--cc={cc}"));
    }
    argv.push(eml.to_string_lossy().into_owned());
    argv
}

/// Counter making scratch directory names unique within a process.
static SCRATCH_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private, self-cleaning directory holding the message file. Lives under the
/// XDG runtime dir (tmpfs, `0700`, wiped at logout) and outside any git repo;
/// removed when dropped.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn create(eml: &str) -> io::Result<Self> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let unique = format!(
            "send-{}-{}-{}",
            std::process::id(),
            now.as_nanos(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let dir = glib::user_runtime_dir().join("koshi").join(unique);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;

        let scratch = Self { dir };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(scratch.eml_path())?;
        file.write_all(eml.as_bytes())?;
        Ok(scratch)
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    fn eml_path(&self) -> PathBuf {
        self.dir.join("reply.eml")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn to_io(error: glib::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn sent_ok_needs_gits_acceptance_line_not_the_exit_code() {
        // Real git send-email stdout after the server accepted the message.
        let success = "reply.eml\n(mbox) Adding to: list@x from line 'To: list@x'\n\
                       OK. Log says:\nServer: 127.0.0.1\nResult: 250\n";
        assert!(sent_ok(success));

        // Real stdout when SMTP auth failed after a cancelled password prompt:
        // git still exits 0, but there is no acceptance line — this is the case
        // that must NOT be reported as sent.
        let cancelled = "reply.eml\n(mbox) Adding to: list@x from line 'To: list@x'\n";
        assert!(!sent_ok(cancelled));
        assert!(!sent_ok(""));
    }

    #[test]
    fn argv_carries_the_hardening_flags_and_recipients() {
        let argv = build_argv(
            "Me <me@x>",
            "list@vger.kernel.org",
            "maint@x.org",
            Path::new("/run/koshi/reply.eml"),
        );
        // The transport is git send-email.
        assert_eq!(&argv[0], "git");
        assert_eq!(&argv[1], "send-email");
        // Recipient control: explicit addresses, every derived Cc suppressed.
        assert!(argv.iter().any(|a| a == "--suppress-cc=all"));
        assert!(argv.iter().any(|a| a == "--to-cmd=true"));
        assert!(argv.iter().any(|a| a == "--no-header-cmd"));
        assert!(argv.iter().any(|a| a == "--from=Me <me@x>"));
        assert!(argv.iter().any(|a| a == "--to=list@vger.kernel.org"));
        assert!(argv.iter().any(|a| a == "--cc=maint@x.org"));
        // No interactive stops, no forced validation/wrapping.
        assert!(argv.iter().any(|a| a == "--confirm=never"));
        assert!(argv.iter().any(|a| a == "--no-annotate"));
        assert!(argv.iter().any(|a| a == "--no-validate"));
        // The message file is the final argument.
        assert_eq!(argv.last().unwrap(), "/run/koshi/reply.eml");
    }

    #[test]
    fn argv_omits_empty_optional_recipients() {
        let argv = build_argv("", "list@x", "", Path::new("/tmp/m.eml"));
        assert!(!argv.iter().any(|a| a.starts_with("--from=")));
        assert!(!argv.iter().any(|a| a.starts_with("--cc=")));
        assert!(argv.iter().any(|a| a == "--to=list@x"));
    }

    #[test]
    fn scratch_writes_a_private_file_and_cleans_up() {
        let dir;
        {
            let scratch = Scratch::create("From: a@b\n\nhi\n").unwrap();
            dir = scratch.dir().to_path_buf();
            let eml = scratch.eml_path();

            // The message is readable back, in a 0600 file inside a 0700 dir.
            assert_eq!(std::fs::read_to_string(&eml).unwrap(), "From: a@b\n\nhi\n");
            let file_mode = std::fs::metadata(&eml).unwrap().permissions().mode() & 0o777;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "message file must be owner-only");
            assert_eq!(dir_mode, 0o700, "scratch dir must be owner-only");
        }
        // Dropped: the directory and its message are gone.
        assert!(!dir.exists(), "scratch dir must be removed on drop");
    }
}
