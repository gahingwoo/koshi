# Releasing Koshi

Koshi is distributed from its own Flatpak repository at
[`dl.nikableh.moe`](https://dl.nikableh.moe), built and published by the
[`Release`](../.github/workflows/release.yml) GitHub Actions workflow. Tagging
`main` with a `v*` tag builds the app, signs it, publishes the OSTree repo to
GitHub Pages, and cuts a GitHub Release with a one-shot `.flatpak` bundle.

## Releasing

1. Bump the version in `Cargo.toml` and add a matching `<release>` entry to
   `data/moe.nikableh.Koshi.metainfo.xml`. Commit on `main`.
2. Tag and push:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The workflow builds, signs, publishes to `dl.nikableh.moe`, and creates the
   GitHub Release. Watch it under the **Actions** tab.

To test the build + Pages publish without cutting a release, trigger the
workflow manually (**Actions → Release → Run workflow**) — the `release` job is
skipped when there's no tag.

## What users run

```sh
flatpak remote-add --if-not-exists koshi https://dl.nikableh.moe/koshi.flatpakrepo
flatpak install koshi moe.nikableh.Koshi
```

Updates come through `flatpak update`. The GNOME runtime is pulled from whatever
remote provides it (usually Flathub); the `koshi.flatpakref` on the release and
at `https://dl.nikableh.moe/koshi.flatpakref` adds Flathub automatically for a
one-click install.

### Verifying provenance

Each release attaches a GitHub build-provenance attestation to the `.flatpak`
bundle. Anyone can confirm the bundle they downloaded was built by this
repository's workflow (not tampered with, not built elsewhere):

```sh
gh attestation verify koshi.flatpak -R nikableh/koshi
```

This is separate from the GPG repo signature — GPG proves the *remote* is ours;
the attestation proves the *bundle* came from our CI. Public repos log the
attestation in Sigstore's public transparency log; while the repo is private it
uses GitHub's private Sigstore instance instead.

## Notes

- **Network during build.** The manifest builds with `--share=network` so Cargo
  can fetch crates. If you later want fully offline/reproducible builds, vendor
  the dependencies with
  [`flatpak-cargo-generator`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo)
  and drop the network share.
- **Repo size.** Each release regenerates the whole OSTree repo (latest version
  only) and the Pages deploy replaces the previous one, so nothing accumulates
  in git history.
