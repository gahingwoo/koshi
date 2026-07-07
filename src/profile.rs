//! The user's identity as git sees it: name, email, and git-send-email
//! settings, including any configured `[sendemail "<name>"]` identities.
//!
//! Everything here is read straight from `git config` — Koshi keeps no
//! account store of its own, so the merged git configuration *is* the
//! account. Parsing is split from the `git` invocation so it can be tested
//! against fixed input.

use std::process::Command;

/// One `key = value` pair from a `[sendemail]` section, carrying the
/// canonical camelCase spelling of the key for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    /// Canonical camelCase key, e.g. `smtpServer`.
    pub key: String,
    pub value: String,
}

/// A configured `git send-email` identity — a `[sendemail "<name>"]`
/// subsection whose settings override the top-level `[sendemail]` ones when
/// the identity is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    /// The address this identity sends as, derived from its `from` (or
    /// failing that its `smtpUser`), for a one-line subtitle.
    pub email: Option<String>,
    pub settings: Vec<Setting>,
}

/// The user's git-derived profile: identity fields plus send-email config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    /// Top-level `[sendemail]` settings.
    pub sendemail: Vec<Setting>,
    /// `[sendemail "<name>"]` subsections.
    pub identities: Vec<Identity>,
    /// The active identity (`sendemail.identity`), if one is selected.
    pub active_identity: Option<String>,
}

impl Profile {
    /// Whether git yielded anything worth showing.
    pub fn is_empty(&self) -> bool {
        self.user_name.is_none()
            && self.user_email.is_none()
            && self.sendemail.is_empty()
            && self.identities.is_empty()
    }

    /// The identity currently selected by `sendemail.identity`, if that name
    /// resolves to a configured subsection.
    pub fn active(&self) -> Option<&Identity> {
        let name = self.active_identity.as_deref()?;
        self.identities.iter().find(|id| id.name == name)
    }

    /// The send-email settings that actually take effect: the top-level
    /// `[sendemail]` values with the active identity's values layered on top,
    /// matching git's own precedence. Keys are returned in a stable,
    /// human-friendly order.
    pub fn effective_sendemail(&self) -> Vec<Setting> {
        let mut merged: Vec<Setting> = self.sendemail.clone();
        if let Some(identity) = self.active() {
            for setting in &identity.settings {
                match merged.iter_mut().find(|s| s.key == setting.key) {
                    Some(existing) => existing.value = setting.value.clone(),
                    None => merged.push(setting.clone()),
                }
            }
        }
        sort_settings(&mut merged);
        merged
    }

    /// The SMTP server (or sendmail command) mail would actually go through,
    /// for the status line.
    pub fn effective_transport(&self) -> Option<String> {
        let settings = self.effective_sendemail();
        let get = |key: &str| {
            settings
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.value.clone())
        };
        get("smtpServer").or_else(|| get("sendmailCmd"))
    }
}

/// Read the merged git configuration (system + global + local) and build a
/// [`Profile`]. Returns an empty profile when git is missing or has nothing
/// configured, so callers never have to distinguish the two.
pub fn load() -> Profile {
    let output = Command::new("git")
        .args(["config", "--list", "-z"])
        .output();
    match output {
        Ok(out) if out.status.success() => parse(&String::from_utf8_lossy(&out.stdout)),
        _ => Profile::default(),
    }
}

/// Point `sendemail.identity` at `name` in the user's global config, so
/// `git send-email` (and Koshi's own view) uses that identity. Returns
/// whether the write succeeded.
pub fn set_active_identity(name: &str) -> bool {
    Command::new("git")
        .args(["config", "--global", "sendemail.identity", name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Parse the null-terminated output of `git config --list -z`. Each record is
/// `key\nvalue`, records are separated by NUL. git lowercases the section and
/// variable names but preserves the case of a subsection (identity) name.
fn parse(config: &str) -> Profile {
    let mut profile = Profile::default();

    for record in config.split('\0').filter(|r| !r.is_empty()) {
        let (key, value) = match record.split_once('\n') {
            Some((key, value)) => (key, value.to_string()),
            // A valueless key (`[section] var` with no `=`) is a boolean true.
            None => (record, "true".to_string()),
        };

        match key {
            "user.name" => profile.user_name = Some(value),
            "user.email" => profile.user_email = Some(value),
            "sendemail.identity" => profile.active_identity = Some(value),
            _ => {
                if let Some(rest) = key.strip_prefix("sendemail.") {
                    ingest_sendemail(&mut profile, rest, value);
                }
            }
        }
    }

    for identity in &mut profile.identities {
        sort_settings(&mut identity.settings);
        identity.email = derive_email(&identity.settings);
    }
    sort_settings(&mut profile.sendemail);

    profile
}

/// Route a `sendemail.*` key (with the `sendemail.` prefix already stripped)
/// to either the top-level settings or an identity subsection. A remaining
/// `subsection.var` shape means an identity; a bare `var` is top-level.
fn ingest_sendemail(profile: &mut Profile, rest: &str, value: String) {
    match rest.rsplit_once('.') {
        Some((subsection, var)) => {
            let Some(setting) = displayable(var, value) else {
                return;
            };
            match profile
                .identities
                .iter_mut()
                .find(|id| id.name == subsection)
            {
                Some(identity) => identity.settings.push(setting),
                None => profile.identities.push(Identity {
                    name: subsection.to_string(),
                    email: None,
                    settings: vec![setting],
                }),
            }
        }
        None => {
            if let Some(setting) = displayable(rest, value) {
                profile.sendemail.push(setting);
            }
        }
    }
}

/// Turn a lowercased variable name into a [`Setting`], skipping secrets we
/// must never render (the SMTP password).
fn displayable(var: &str, value: String) -> Option<Setting> {
    if var == "smtppass" {
        return None;
    }
    Some(Setting {
        key: canonical_key(var),
        value,
    })
}

/// The address an identity sends as: prefer the `<addr>` inside its `from`,
/// fall back to `smtpUser`.
fn derive_email(settings: &[Setting]) -> Option<String> {
    let get = |key: &str| settings.iter().find(|s| s.key == key).map(|s| &s.value);
    if let Some(from) = get("from") {
        if let (Some(start), Some(end)) = (from.find('<'), from.rfind('>'))
            && start < end
        {
            return Some(from[start + 1..end].trim().to_string());
        }
        return Some(from.clone());
    }
    get("smtpUser").cloned()
}

/// Order settings by a curated display sequence (server details first, then
/// addressing, then behaviour flags); keys outside the list keep their
/// original relative order after the known ones.
fn sort_settings(settings: &mut [Setting]) {
    settings.sort_by_key(|s| display_rank(&s.key));
}

fn display_rank(key: &str) -> usize {
    const ORDER: &[&str] = &[
        "smtpServer",
        "smtpServerPort",
        "smtpServerOption",
        "smtpEncryption",
        "smtpUser",
        "smtpDomain",
        "smtpSslCertPath",
        "sendmailCmd",
        "from",
        "envelopeSender",
        "to",
        "cc",
        "toCmd",
        "ccCmd",
        "suppressCc",
        "suppressFrom",
        "confirm",
        "chainReplyTo",
        "annotate",
        "signedOffByCc",
        "signedOffCc",
        "transferEncoding",
        "thread",
    ];
    ORDER
        .iter()
        .position(|k| *k == key)
        .unwrap_or(ORDER.len())
}

/// The canonical camelCase spelling git documents for a lowercased send-email
/// variable; unknown keys fall through unchanged.
fn canonical_key(var: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("smtpserver", "smtpServer"),
        ("smtpserverport", "smtpServerPort"),
        ("smtpserveroption", "smtpServerOption"),
        ("smtpencryption", "smtpEncryption"),
        ("smtpuser", "smtpUser"),
        ("smtpdomain", "smtpDomain"),
        ("smtpsslcertpath", "smtpSslCertPath"),
        ("sendmailcmd", "sendmailCmd"),
        ("from", "from"),
        ("envelopesender", "envelopeSender"),
        ("to", "to"),
        ("cc", "cc"),
        ("tocmd", "toCmd"),
        ("cccmd", "ccCmd"),
        ("suppresscc", "suppressCc"),
        ("suppressfrom", "suppressFrom"),
        ("confirm", "confirm"),
        ("chainreplyto", "chainReplyTo"),
        ("annotate", "annotate"),
        ("signedoffbycc", "signedOffByCc"),
        ("signedoffcc", "signedOffCc"),
        ("transferencoding", "transferEncoding"),
        ("thread", "thread"),
        ("multiedit", "multiEdit"),
        ("validate", "validate"),
        ("xmailer", "xMailer"),
        ("assume8bitencoding", "assume8bitEncoding"),
        ("forbidsendmailvariables", "forbidSendmailVariables"),
    ];
    KNOWN
        .iter()
        .find(|(lower, _)| *lower == var)
        .map(|(_, camel)| camel.to_string())
        .unwrap_or_else(|| var.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the NUL-terminated form `git config --list -z` emits from
    /// `(key, value)` pairs.
    fn config_z(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(k, v)| format!("{k}\n{v}\0"))
            .collect()
    }

    #[test]
    fn reads_user_and_top_level_sendemail() {
        let profile = parse(&config_z(&[
            ("user.name", "Nika Krasnova"),
            ("user.email", "nika@nikableh.moe"),
            ("sendemail.smtpserver", "smtp.purelymail.com"),
            ("sendemail.smtpserverport", "465"),
            ("sendemail.smtpencryption", "ssl"),
        ]));

        assert_eq!(profile.user_name.as_deref(), Some("Nika Krasnova"));
        assert_eq!(profile.user_email.as_deref(), Some("nika@nikableh.moe"));
        assert!(profile.identities.is_empty());
        assert_eq!(profile.active_identity, None);
        // Sorted into display order: server, port, encryption.
        let keys: Vec<&str> = profile.sendemail.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["smtpServer", "smtpServerPort", "smtpEncryption"]);
    }

    #[test]
    fn groups_identity_subsections_and_derives_email() {
        let profile = parse(&config_z(&[
            ("sendemail.identity", "work"),
            ("sendemail.smtpserver", "smtp.default"),
            ("sendemail.work.smtpserver", "smtp.baylibre.com"),
            ("sendemail.work.from", "Nika Bleh <nika.bleh@baylibre.com>"),
            ("sendemail.personal.smtpuser", "nika@nikableh.moe"),
        ]));

        assert_eq!(profile.active_identity.as_deref(), Some("work"));
        assert_eq!(profile.identities.len(), 2);

        let work = profile.identities.iter().find(|i| i.name == "work").unwrap();
        assert_eq!(work.email.as_deref(), Some("nika.bleh@baylibre.com"));

        // No `from`, so the personal identity's email falls back to smtpUser.
        let personal = profile
            .identities
            .iter()
            .find(|i| i.name == "personal")
            .unwrap();
        assert_eq!(personal.email.as_deref(), Some("nika@nikableh.moe"));
    }

    #[test]
    fn subsection_casing_is_preserved() {
        // git preserves subsection case while lowercasing section/variable.
        let profile = parse(&config_z(&[("sendemail.Work.smtpserver", "smtp.example.com")]));
        assert_eq!(profile.identities[0].name, "Work");
        assert_eq!(profile.identities[0].settings[0].key, "smtpServer");
    }

    #[test]
    fn active_identity_overrides_top_level_in_effective_view() {
        let profile = parse(&config_z(&[
            ("sendemail.identity", "work"),
            ("sendemail.smtpserver", "smtp.default"),
            ("sendemail.smtpencryption", "tls"),
            ("sendemail.work.smtpserver", "smtp.baylibre.com"),
            ("sendemail.work.smtpserverport", "465"),
        ]));

        let effective = profile.effective_sendemail();
        let get = |key: &str| {
            effective
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.value.as_str())
        };
        // Identity wins for the server, adds the port, keeps the untouched
        // top-level encryption.
        assert_eq!(get("smtpServer"), Some("smtp.baylibre.com"));
        assert_eq!(get("smtpServerPort"), Some("465"));
        assert_eq!(get("smtpEncryption"), Some("tls"));
        assert_eq!(profile.effective_transport().as_deref(), Some("smtp.baylibre.com"));
    }

    #[test]
    fn effective_view_without_active_identity_is_top_level_only() {
        let profile = parse(&config_z(&[
            ("sendemail.smtpserver", "smtp.default"),
            ("sendemail.work.smtpserver", "smtp.baylibre.com"),
        ]));
        // Identity exists but is not selected, so it does not take effect.
        assert_eq!(profile.active(), None);
        assert_eq!(
            profile.effective_transport().as_deref(),
            Some("smtp.default")
        );
    }

    #[test]
    fn smtp_password_is_never_surfaced() {
        let profile = parse(&config_z(&[
            ("sendemail.smtppass", "hunter2"),
            ("sendemail.smtpuser", "nika@nikableh.moe"),
        ]));
        assert!(
            profile
                .sendemail
                .iter()
                .all(|s| s.key != "smtpPass" && s.value != "hunter2"),
            "the SMTP password must not appear in the profile"
        );
    }

    #[test]
    fn valueless_boolean_keys_default_to_true() {
        // `git config --list -z` emits a lone key (no `\n`) for a bare boolean.
        let profile = parse("sendemail.annotate\0");
        assert_eq!(profile.sendemail[0].key, "annotate");
        assert_eq!(profile.sendemail[0].value, "true");
    }

    #[test]
    fn empty_config_yields_empty_profile() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn active_returns_none_when_pointer_is_dangling() {
        let profile = parse(&config_z(&[
            ("sendemail.identity", "ghost"),
            ("sendemail.work.smtpserver", "smtp.baylibre.com"),
        ]));
        // sendemail.identity names an identity that isn't configured.
        assert_eq!(profile.active(), None);
    }
}
