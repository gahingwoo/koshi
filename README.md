# koshi

[![Please do not theme this app](https://stopthemingmy.app/badge.svg)](https://stopthemingmy.app)

<p align="center">
  <img src="data/icons/scalable/apps/moe.nikableh.Koshi.svg" width="128" alt="Koshi icon">
  <br>
  <i>Read and reply to kernel mailing lists.</i>
</p>

Koshi is a native GTK4 + libadwaita app for browsing, searching, and replying
to the mailing lists archived on [lore.kernel.org](https://lore.kernel.org).

## Installing

Koshi is distributed as a Flatpak from its own repository:

```sh
flatpak remote-add --if-not-exists koshi https://dl.nikableh.moe/koshi.flatpakrepo
flatpak install koshi moe.nikableh.Koshi
```

Updates arrive through `flatpak update`. For a one-off install without adding
the remote, download the `.flatpak` bundle from the [latest release] and install
it:

```sh
flatpak install koshi.flatpak
```

You can verify the bundle's build provenance before installing:

```sh
gh attestation verify koshi.flatpak -R nikableh/koshi
```

[latest release]: https://github.com/nikableh/koshi/releases/latest

## Building

```sh
cargo run -r
```

or

```sh
nix run
```

### Flatpak

From zero: install `flatpak` and `flatpak-builder` with your distribution's
package manager (on NixOS, `services.flatpak.enable = true`), then add Flathub
if you have not already:

```sh
flatpak remote-add --if-not-exists --user \
  flathub https://dl.flathub.org/repo/flathub.flatpakrepo
```

Install the runtime, SDK, and the Rust SDK extension the manifest builds
against:

```sh
flatpak install --user flathub \
  org.gnome.Platform//50 \
  org.gnome.Sdk//50 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
```

Build, install, and run:

```sh
flatpak-builder --user --install --force-clean \
  --state-dir=target/flatpak-builder target/flatpak-build \
  build-aux/moe.nikableh.Koshi.json
flatpak run moe.nikableh.Koshi
```

**On NixOS, run `flatpak-builder` from inside `nix develop`.** flatpak
validates the app's exported icons with the host's gdk-pixbuf loaders, and a
bare shell without librsvg's SVG loader rejects every SVG icon with
`Format not recognized`. The dev shell (`flake.nix`) provides the loader:

```sh
nix develop -c flatpak-builder --user --install --force-clean \
  --state-dir=target/flatpak-builder target/flatpak-build \
  build-aux/moe.nikableh.Koshi.json
```

The manifest fetches crates during the build (`--share=network` in
`build-args`), so it is for local builds — publishing to Flathub would need a
generated `cargo-sources.json` instead.

Inside the sandbox Koshi still uses the *host's* git for identity and
sending — invocations cross over via `flatpak-spawn --host`
(`--talk-name=org.freedesktop.Flatpak`), so your git config, credential
helpers, and `git send-email` setup work unchanged.

## Releasing

Publishing is automated: tag `main` with a `v*` version and GitHub Actions
builds, GPG-signs, and publishes the Flatpak to
[dl.nikableh.moe](https://dl.nikableh.moe), then creates a GitHub Release with a
`.flatpak` bundle.

## License

This project is under the [GPL-3.0-only] license.

[GPL-3.0-only]: https://opensource.org/license/gpl-3-0
