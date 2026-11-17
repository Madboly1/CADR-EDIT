# Releasing CADR Studio

The release workflow is `.github/workflows/release.yml`. It builds when a version tag matching
`v*` is pushed, then publishes the generated GitHub Release only after the reusable build workflow
succeeds and the draft contains at least one asset. A manual `workflow_dispatch` run is available
for testing the pipeline; it does not publish a release.

The updater checks this repository (`Madboly1/CADR-EDIT`) for releases. The existing `filmcraft`
package and asset stem is intentionally retained for compatibility with current binaries and
installers; it is not the product's public name. User-facing product name: **CADR Studio**.

## Cutting a release

1. Review and test the exact commit intended for release.
2. Set the workspace version in the root `Cargo.toml` and `Cargo.lock`:

   ```sh
   cargo xtask version
   cargo xtask version set 0.20.2
   ```

3. Commit the version bump and release notes through the normal review process.
4. Create and push a matching version tag (for example `v0.20.2`) from that reviewed commit.
   The tag is the release trigger; a push to a `release` branch is not.
5. Wait for the Release workflow to finish. It builds the platform packages, creates a draft,
   checks that the generated tag matches the pushed tag and that at least one asset exists, then
   publishes the release. Inspect the Actions run and test the Windows installer before announcing.
6. For a release candidate, use a pre-release tag such as `v0.20.2-rc.1`.

A failed build or publish validation prevents the final publish job from running. Do not manually
publish an incomplete draft or tag an unreviewed commit.

**Test runs:** *Actions › Release › Run workflow* runs the pipeline manually with an optional
version override. It does not publish a release. Use it only when you intend to spend the CI time
building the platform packages.

## What gets built

| Platform | Artifacts | Built on |
|---|---|---|
| macOS 11+ (universal: Apple silicon and Intel) | `filmcraft-<v>-macos-universal.dmg`, `filmcraft-cli-<v>-macos-universal.zip` | `macos-15` |
| Windows x64 | `filmcraft-<v>-windows-x64.msi`, `filmcraft-<v>-windows-x64-portable.zip` | `windows-latest` |
| Windows x86 (32-bit) | `filmcraft-<v>-windows-x86.msi`, `filmcraft-<v>-windows-x86-portable.zip` | `windows-latest` |
| Windows on ARM64 | `filmcraft-<v>-windows-arm64.msi`, `filmcraft-<v>-windows-arm64-portable.zip` | `windows-latest` (cross-compiled) |
| Linux x86_64 | `filmcraft-<v>-linux-x86_64.{AppImage,deb,rpm,tar.gz}` | `ubuntu-22.04` |
| Linux aarch64 | `filmcraft-<v>-linux-aarch64.{AppImage,deb,rpm,tar.gz}` | `ubuntu-22.04-arm` |
| Linux riscv64 | `filmcraft-<v>-linux-riscv64.tar.gz` | `ubuntu-24.04`, cross-compiled (glibc 2.39+; CLI smoke-tested under QEMU) |
| AppImage updates | `filmcraft-<v>-linux-{x86_64,aarch64}.AppImage.zsync` | with the AppImage |
| Flatpak | `filmcraft-<v>-linux-x86_64.flatpak`, `filmcraft-<v>-linux-aarch64.flatpak` | `ubuntu-24.04`, `ubuntu-24.04-arm` (repackaged Linux tarball) |
| FreeBSD 14 x86_64 | `filmcraft-<v>-freebsd-x86_64.tar.gz` | FreeBSD 14.3 VM on `ubuntu-latest` |
| Web | `filmcraft-web-<v>.zip`, a static site (see [`packaging/web/README.md`](../packaging/web/README.md)) | `ubuntu-latest` |

`filmcraft --version` and `filmcraft-cli --version` print the version from `Cargo.toml`.

**Fonts.** Every build job checks out [craft-fonts](https://github.com/storytold/craft-fonts) at a
pinned commit (`craft-fonts-ref` in `.github/workflows/release.yml` and `freebsd.yml`) and builds
with `CRAFT_FONTS_DIR` and `CRAFT_FONTS_REQUIRED=1`, so releases embed its fonts and fail
rather than ship without them (the web build embeds only the UI font, to stay small). The packages
carry each embedded font's licence (`copy_font_licences` in `packaging/env.sh`). Bump the pins
deliberately, in every job at once.

### macOS

`packaging/macos/package.sh` builds `aarch64-apple-darwin` and `x86_64-apple-darwin` with
`MACOSX_DEPLOYMENT_TARGET=11.0`, joins them with `lipo` and assembles `FilmCraft.app`:

- `Info.plist` is generated from `Info.plist.in` (bundle id `ai.storyteller.filmcraft`, the
  version and the build commit).
- **Signing** uses the hardened runtime and a secure timestamp, with the entitlements in
  `entitlements.plist`. The Developer ID certificate is imported into a temporary keychain by
  `packaging/macos/import-cert.sh`.
- **Notarization:** the app is sent with `xcrun notarytool submit`, then the ticket is stapled
  and checked with `stapler validate`. The app goes on a DMG (`hdiutil makehybrid`), which is
  signed and notarized too. The universal `filmcraft-cli` is signed the same way, zipped, and the zip is notarized.
  Its Finder window (background, icon size and positions) comes from
  [`packaging/macos/dmg/`](../packaging/macos/dmg/README.md), and its volume is named `FilmCraft`
  without the version, which the window's background needs; the DMG file name keeps the version.

Locally, without certificates, the script signs ad hoc and skips notarization, which is enough to
check the bundle and the DMG on your own Mac (`packaging/macos/package.sh`).

### Windows

`packaging/windows/package.ps1 -Arch x64|x86|arm64` builds with `+crt-static`, so neither the MSI
nor the portable zip needs the Visual C++ redistributable.

- `filmcraft.wxs` (WiX) is a dual-scope package: per-user by default into
  `%LOCALAPPDATA%\Programs\ArtCraft\FilmCraft`, per-machine into `Program Files\ArtCraft\FilmCraft` (`Program Files (x86)` for the 32-bit build)
  (see `packaging/windows/INSTALLATION.md`). It has a Start Menu shortcut and
  an App Paths entry (Win+R `filmcraft`). Same-version upgrades are allowed, so release candidates
  replace each other.
- The ARM64 build is cross-compiled on the x64 runner. `.github/workflows/windows-arm64.yml`
  installs that MSI on a Windows 11 ARM64 runner, runs `filmcraft-cli --version` natively and
  uninstalls.
- **Signing:** `packaging/windows/sign.ps1` signs executables and the `.msi` when either a `.pfx`
  certificate (`WINDOWS_CERTIFICATE`, base64, and `WINDOWS_CERTIFICATE_PASSWORD`) or Azure Trusted
  Signing credentials (`AZURE_*`) are configured. Without a certificate, Windows packages still
  build unsigned. The in-app updater cannot silently replace an unsigned Windows executable; users
  must use the release-page/download fallback and install updates manually. This is the intended
  no-certificate path until signing is justified by revenue.

### Linux

`packaging/linux/package.sh` builds the release binaries and packages them as an AppImage, a
`.deb` and an `.rpm` (with [nfpm](https://nfpm.goreleaser.com), from `nfpm.yaml`) and a plain
`.tar.gz` tree. The packages install both programs, the desktop entry, the AppStream metainfo, the
icons and the licence files.

The Linux tarball includes `install.sh`. Extract it, open a terminal in the extracted
directory and run `./install.sh` to install both programs and their desktop integration
under `~/.local` (no administrator permissions needed). The application-menu entry uses
the installed binary's absolute path; add `~/.local/bin` to `PATH` for terminal commands.
Run the installer again to update. For a system-wide installation, use
`sudo ./install.sh --prefix /usr/local`. A custom absolute `--prefix` is also supported;
its `share` directory must be in the desktop's data search path to appear in the menu.
Prefixes containing `=`, `%` or control characters are rejected for desktop-launcher compatibility.

The jobs run on `ubuntu-22.04`, the oldest GitHub-hosted image, so the binaries only need
glibc 2.35 or newer: Ubuntu 22.04+, Debian 12+, Fedora 36+ and RHEL 10. The deb (`libc6 (>= 2.35)`)
and the rpm (`glibc >= 2.35`) both declare that floor, so older systems refuse the install. Moving
the job to a newer image raises the floor, so do it deliberately and update `nfpm.yaml` with it.
After packaging, the job prints the `.deb`'s metadata and contents, runs `ldd` on the binary and
runs each AppImage with `--version`.

**AppImage updates.** Each AppImage embeds update information,
`gh-releases-zsync|Madboly1|CADR-EDIT|latest|filmcraft-*-linux-<arch>.AppImage.zsync`, and
`appimagetool` writes the matching `.zsync` beside it (the job installs `zsync` for
`zsyncmake`). [AppImageUpdate](https://github.com/AppImageCommunity/AppImageUpdate) and
AppImageLauncher then fetch only the changed blocks from the newest published (non-pre-release)
release. The job checks both with `--appimage-updateinformation` and `test -s`.

**Flatpak.** The `flatpak` jobs (x86_64 on `ubuntu-24.04`, aarch64 on `ubuntu-24.04-arm`) run
`packaging/linux/flatpak-bundle.sh` on the Linux job's tarball: it installs the binaries with
`packaging/linux/flatpak/ai.storyteller.filmcraft.bundle.yml` into a single-file bundle,
`filmcraft-<v>-linux-<arch>.flatpak` (no Rust build, so it takes a minute), then installs it and
runs `filmcraft-cli --version` in the sandbox. Users install it with
`flatpak install --user <file>`; the freedesktop runtime comes from Flathub.
`packaging/linux/flatpak/ai.storyteller.filmcraft.yml` is the from-source manifest for a Flathub
submission (its header says how to build it). Both manifests must keep the same runtime and
`finish-args`, and install the same programs: the app and `filmcraft-cli`, which runs the
headless commands and the MCP server in the sandbox
(`flatpak run --command=filmcraft-cli ai.storyteller.filmcraft mcp`). packaging-lint checks this.

### FreeBSD

GitHub has no FreeBSD runners, so the `freebsd` job builds in a FreeBSD 14.3 VM
([vmactions/freebsd-vm](https://github.com/vmactions/freebsd-vm), pinned by commit) on
`ubuntu-latest`. `packaging/freebsd/package.sh` builds both programs and packs a
`/usr/local`-style tree as `filmcraft-<v>-freebsd-x86_64.tar.gz`; install it with
`tar -xzf filmcraft-<v>-freebsd-x86_64.tar.gz --strip-components 1 -C /usr/local`. The build needs
the packages listed in the job (winit/wgpu, GTK 3 for the file dialogs, fontconfig, and
`alsa-lib` for audio through cpal). `.github/workflows/freebsd.yml` builds and packages the same
way nightly, so a FreeBSD break shows up before a release. `package.sh --dry-run` checks the
tree's layout with stub binaries on any OS (packaging-lint runs it).

### Web

The web job builds `apps/filmcraft-web` with `cargo xtask web` (the `wasm-bindgen` CLI must match
the crate's version exactly, `WASM_BINDGEN` in `xtask/src/main.rs`) and zips the static site with
`packaging/web/package.sh`. [`packaging/web/README.md`](../packaging/web/README.md) covers hosting.

## The draft release

The reusable release workflow waits for platform builds, downloads their artifacts, writes
`SHA256SUMS.txt` and creates the draft release. The repository's final publish job verifies that
the draft tag matches the pushed tag and that at least one asset exists before publishing. If the
release is missing or has no assets, the workflow fails rather than publishing an empty release.

## Secrets

The macOS, Windows and draft-release jobs run in the `release` environment, which only the
`release` branch can use and which holds the signing secrets. The other jobs sign nothing and get
no secrets. Signing secrets are optional for Windows: without them, Windows artifacts are unsigned and the
updater uses the manual download fallback. Other platform signing and notarization requirements
depend on the corresponding platform workflow.

| Secret | Used for |
|---|---|
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD` | the Developer ID certificate (base64 `.p12`), imported into a temporary keychain |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | notarization (`APPLE_PASSWORD` is an app-specific password) |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` | Windows signing with a `.pfx` |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_CERT_PROFILE` | Windows signing with Azure Trusted Signing |
