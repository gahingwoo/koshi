//! Running inside a Flatpak sandbox.
//!
//! Koshi's account and transport are the *host's* git — the sandbox runtime
//! ships no git, and a bundled one could not run the user's credential
//! helpers. So under Flatpak every git invocation crosses the boundary
//! through `flatpak-spawn --host` (the session-helper portal, granted by
//! `--talk-name=org.freedesktop.Flatpak` in the manifest), which has two
//! consequences this module centralises:
//!
//! - The portal starts the host command with the *user session's*
//!   environment, not the sandbox's. Anything git must see (`GIT_ASKPASS`,
//!   `LC_ALL=C`, …) has to be forwarded explicitly as `--env=` flags.
//! - Every path handed to git — the message file, the working directory, the
//!   askpass program — must exist *on the host*, and the portal refuses even
//!   to start a command whose cwd does not. The one directory both sides see
//!   at the same path is the per-app runtime dir
//!   `$XDG_RUNTIME_DIR/app/$FLATPAK_ID` — but only by its *logical* path. That
//!   dir is a bind mount whose internal path is `/run/flatpak/app/$FLATPAK_ID`,
//!   so a child that `chdir`s into it reports that internal cwd via `getcwd`,
//!   and `flatpak-spawn` would forward *that* to the host, where it does not
//!   exist. So the host cwd must be pinned with an explicit `--directory=`
//!   naming the logical path, never left to inherited-cwd forwarding.

use std::path::{Path, PathBuf};
use std::process::Command;

use gtk::glib;

/// The sandbox's application id, or `None` when not running under Flatpak.
/// `/.flatpak-info` is the canonical sandbox marker; `FLATPAK_ID` names the
/// app (`moe.nikableh.Koshi`).
pub fn app_id() -> Option<String> {
    if Path::new("/.flatpak-info").exists() {
        std::env::var("FLATPAK_ID").ok()
    } else {
        None
    }
}

/// The base directory for runtime files that host processes must also see:
/// the per-app dir inside a sandbox (bind-mounted at the same path on both
/// sides, shared by all instances of the app), the plain user runtime dir
/// otherwise.
pub fn shared_runtime_dir() -> PathBuf {
    match app_id() {
        Some(id) => glib::user_runtime_dir().join("app").join(id),
        None => glib::user_runtime_dir(),
    }
}

/// A `git` command, run directly or — under Flatpak — on the host through
/// `flatpak-spawn`. The host branch pins the cwd to `/` because the portal
/// errors out on a cwd the host does not have (the sandbox cwd usually
/// isn't); callers of this (config reads and writes, credential eviction)
/// must not see repo-local config anyway, so losing the inherited cwd is a
/// feature.
pub fn git_command() -> Command {
    match app_id() {
        Some(_) => {
            let mut command = Command::new("flatpak-spawn");
            command.arg("--host").arg("git").current_dir("/");
            command
        }
        None => Command::new("git"),
    }
}

/// Wrap a `git …` argv to run on the host when sandboxed, forwarding `env`
/// explicitly and pinning the host cwd to `cwd` — the portal forwards neither
/// the sandbox environment nor a usable cwd (see module docs). `cwd` must name
/// a directory by its logical host path (e.g. under [`shared_runtime_dir`]).
/// Outside Flatpak the argv is returned untouched, and both `env` and the cwd
/// are the launcher's business.
pub fn host_git_argv(argv: Vec<String>, env: &[(String, String)], cwd: &Path) -> Vec<String> {
    match app_id() {
        Some(_) => wrap_host_argv(argv, env, cwd),
        None => argv,
    }
}

/// The pure wrapping [`host_git_argv`] applies inside a sandbox, split out so
/// it is testable without one.
fn wrap_host_argv(argv: Vec<String>, env: &[(String, String)], cwd: &Path) -> Vec<String> {
    let mut wrapped = vec![
        "flatpak-spawn".to_string(),
        "--host".to_string(),
        // Pin the host cwd to the logical path; without this the portal
        // forwards the child's `getcwd`, which is the bind mount's internal
        // `/run/flatpak/...` path and does not exist on the host.
        format!("--directory={}", cwd.display()),
    ];
    wrapped.extend(
        env.iter()
            .map(|(key, value)| format!("--env={key}={value}")),
    );
    wrapped.extend(argv);
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_argv_pins_cwd_and_forwards_env_before_the_command() {
        let argv = vec!["git".to_string(), "send-email".to_string()];
        let env = vec![
            ("LC_ALL".to_string(), "C".to_string()),
            (
                "GIT_ASKPASS".to_string(),
                "/run/user/1/app/x/askpass".to_string(),
            ),
        ];
        let wrapped = wrap_host_argv(argv, &env, Path::new("/run/user/1/app/x/koshi/send-0"));
        assert_eq!(
            wrapped,
            [
                "flatpak-spawn",
                "--host",
                // The host cwd is pinned explicitly (never inherited), and every
                // env var git must see is forwarded, all before the command.
                "--directory=/run/user/1/app/x/koshi/send-0",
                "--env=LC_ALL=C",
                "--env=GIT_ASKPASS=/run/user/1/app/x/askpass",
                "git",
                "send-email",
            ]
        );
    }
}
