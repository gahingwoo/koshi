use std::collections::HashMap;

use gtk::gio;

use super::{BASE_URL, Error, fetch, gunzip_to_string};

pub struct Inbox {
    pub slug: String,
    pub description: String,
    /// Unix timestamp of the most recent activity across the inbox's epochs.
    pub modified: i64,
}

/// The aggregate pseudo-inbox; a real endpoint, but absent from the manifest.
pub const ALL_SLUG: &str = "all";

pub async fn fetch_inboxes(cancellable: &gio::Cancellable) -> Result<Vec<Inbox>, Error> {
    let bytes = fetch(&format!("{BASE_URL}/manifest.js.gz"), cancellable).await?;
    let json = gunzip_to_string(&bytes)?;
    let mut inboxes = parse_manifest(&json)?;
    inboxes.insert(
        0,
        Inbox {
            slug: ALL_SLUG.to_string(),
            description: "Every list archived on lore.kernel.org".to_string(),
            modified: 0,
        },
    );
    Ok(inboxes)
}

/// Parse lore's manifest: a JSON object keyed by git paths like
/// `/<slug>/git/<epoch>.git`. Epochs of one inbox are merged, keeping the
/// most recent `modified` timestamp.
pub fn parse_manifest(json: &str) -> Result<Vec<Inbox>, Error> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| Error::Parse(err.to_string()))?;
    let entries = value
        .as_object()
        .ok_or_else(|| Error::Parse("manifest is not a JSON object".to_string()))?;

    let mut merged: HashMap<&str, Inbox> = HashMap::new();
    for (key, entry) in entries {
        let Some(slug) = key.strip_prefix('/').and_then(|k| {
            let (slug, epoch) = k.split_once("/git/")?;
            epoch.ends_with(".git").then_some(slug)
        }) else {
            continue;
        };
        let modified = entry
            .get("modified")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let description = entry
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        let inbox = merged.entry(slug).or_insert_with(|| Inbox {
            slug: slug.to_string(),
            description: strip_epoch_suffix(description).to_string(),
            modified,
        });
        inbox.modified = inbox.modified.max(modified);
    }

    let mut inboxes: Vec<Inbox> = merged.into_values().collect();
    inboxes.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(inboxes)
}

/// Manifest descriptions end in " [epoch N]"; the epoch split is an archive
/// storage detail not worth showing.
fn strip_epoch_suffix(description: &str) -> &str {
    match description.rfind(" [epoch ") {
        Some(pos) if description.ends_with(']') => &description[..pos],
        _ => description,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "/bpf/git/0.git": {"description": "BPF List [epoch 0]", "modified": 100, "fingerprint": "aa"},
        "/bpf/git/1.git": {"description": "BPF List [epoch 1]", "modified": 200, "fingerprint": "bb"},
        "/lkml/git/0.git": {"description": "LKML Archive on lore.kernel.org", "modified": 150},
        "not-a-git-path": {"description": "ignored"}
    }"#;

    #[test]
    fn epochs_are_merged_keeping_latest_modified() {
        let inboxes = parse_manifest(MANIFEST).unwrap();
        assert_eq!(inboxes.len(), 2);
        assert_eq!(inboxes[0].slug, "bpf");
        assert_eq!(inboxes[0].modified, 200);
    }

    #[test]
    fn epoch_suffix_is_stripped_from_descriptions() {
        let inboxes = parse_manifest(MANIFEST).unwrap();
        assert_eq!(inboxes[0].description, "BPF List");
        assert_eq!(inboxes[1].description, "LKML Archive on lore.kernel.org");
    }

    #[test]
    fn inboxes_are_sorted_by_slug() {
        let inboxes = parse_manifest(MANIFEST).unwrap();
        let slugs: Vec<&str> = inboxes.iter().map(|i| i.slug.as_str()).collect();
        assert_eq!(slugs, ["bpf", "lkml"]);
    }

    #[test]
    fn invalid_json_is_a_parse_error() {
        assert!(matches!(parse_manifest("nope"), Err(Error::Parse(_))));
    }
}
