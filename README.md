# breakd

`breakd` is a Wayland-native break reminder for Arch Linux and Hyprland. A headless daemon owns scheduling and persistence. Break UI runs in a supervised GTK4 layer-shell child that creates one surface per selected Wayland output.

## Current scope

Implemented:

- Mini and long break cadence with durable state.
- Pre-break desktop notifications.
- Pause, timed pause, resume, reset, skip, postpone, and manual breaks.
- Delayed or entire-break strict mode.
- `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` recovery across clock changes and suspend.
- logind sleep/lock signals and `ext-idle-notify-v1` natural-break detection.
- Per-output `wlr-layer-shell` overlays with hot-plug reconciliation.
- All, focused, cursor, configured, application-primary, and dim-all/content-one display modes.
- Connector/EDID monitor identities from direct Hyprland IPC.
- Unix-socket CLI with same-UID authentication.
- D-Bus notifications, systemd user service, and diagnostics.

Not yet implemented:

- StatusNotifierItem tray.
- Portal global shortcuts. Hyprland CLI bindings are recommended.
- Automatic full-screen postponement. The dependable behavior is to show on the overlay layer.
- A graphical preferences editor.

## Requirements

Arch packages:

```text
gtk4
gtk4-layer-shell
wayland
wayland-protocols
rust
pkgconf
```

The program runs as the current user. It does not read `/dev/input`, require the `input` group, or use root privileges.

## Build

```bash
cargo build --release --workspace
cargo test --workspace
```

Install for the current user:

```bash
install -Dm755 target/release/breakd ~/.local/bin/breakd
install -Dm644 packaging/systemd/breakd-local.service ~/.config/systemd/user/breakd.service
install -Dm600 config.example.toml ~/.config/breakd/config.toml
systemctl --user daemon-reload
systemctl --user enable --now breakd.service
```

The service requires `WAYLAND_DISPLAY`, `HYPRLAND_INSTANCE_SIGNATURE`, and the D-Bus address in the systemd user-manager environment. `breakd doctor` reports missing values. Most Hyprland/Omarchy sessions already import them.

## Commands

```text
breakd status [--json]
breakd pause [30m]
breakd resume
breakd reset
breakd skip
breakd postpone
breakd mini
breakd long
breakd toggle
breakd reload
breakd outputs [--json]
breakd doctor [--json]
breakd example-config
```

The control socket is `$XDG_RUNTIME_DIR/breakd/control.sock`, mode `0600`. `status`, `outputs`, and `doctor` support machine-readable JSON.

## Configuration

Copy `config.example.toml` to `$XDG_CONFIG_HOME/breakd/config.toml`. If the file is absent, built-in defaults are used. `breakd reload` validates the complete file before replacing active settings.

Scheduler, notification, display, and content changes apply to subsequent events and overlays. Restart the daemon after changing idle-monitor or logging settings because those integrations own long-lived subscriptions.

Monitor selectors shown by `breakd outputs` use:

```text
edid:<make>:<model>:<serial>
connector:<name>
```

EDID identity is preferred. Connector identity is the fallback for displays without a usable serial. Display-array indices are never persisted.

`display.pointer_mode = "block"` is the default and prevents pointer input from reaching applications behind any overlay surface. `controls` only captures the content panel, while `none` makes the complete overlay click-through.

## Hyprland integration

Hyprland 0.55+ Lua bindings:

```lua
hl.bind("SUPER + SHIFT + B", hl.dsp.exec_cmd("breakd toggle"))
hl.bind("SUPER + SHIFT + S", hl.dsp.exec_cmd("breakd skip"))
hl.bind("SUPER + SHIFT + P", hl.dsp.exec_cmd("breakd postpone"))
hl.bind("SUPER + SHIFT + M", hl.dsp.exec_cmd("breakd mini"))
hl.bind("SUPER + SHIFT + L", hl.dsp.exec_cmd("breakd long"))
hl.bind("SUPER + SHIFT + R", hl.dsp.exec_cmd("breakd reset"))
```

Optional layer rule:

```lua
hl.layer_rule({
  match = { namespace = "^breakd-overlay$" },
  no_anim = true,
})
```

Check existing bindings before adding these. Reload and validate with:

```bash
hyprctl reload
hyprctl configerrors
```

## State and recovery

State is stored at `$XDG_STATE_HOME/breakd/state.json` using a temporary file, `fsync`, and atomic rename. Same-boot recovery uses persisted monotonic/boottime deadlines. A changed kernel boot ID starts a fresh work interval.

The daemon remains authoritative if overlay creation fails. Overlay children are killed when a break ends, and dropping their single Wayland connection removes all associated output surfaces.

Logs are written to stdout/stderr and captured by the user journal:

```bash
journalctl --user -u breakd.service -f
```

## Development checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

Real multi-monitor behavior must still be checked under Hyprland. Nested wlroots compositors are useful for lifecycle tests but do not replace target-session validation.

## Releases and AUR

The release workflow follows the same model as `codeforces-tui`:

1. Release Please maintains a release pull request from conventional commits.
2. Merging that pull request creates a `vX.Y.Z` tag and GitHub release.
3. CI builds a deterministic source archive and tests an Arch package in an `archlinux:base-devel` container.
4. CI uploads the source archive, binary package, `PKGBUILD`, and `.SRCINFO` to the release.
5. CI pushes the generated `PKGBUILD` and `.SRCINFO` to `ssh://aur@aur.archlinux.org/breakd.git`.

`packaging/arch/PKGBUILD` is the versioned template. The release build replaces its `SKIP` marker with the source archive's SHA-256 checksum before publishing to the AUR.

One-time repository setup:

```bash
gh secret set AUR_SSH_PRIVATE_KEY \
  --repo simonwinther/breakd < ~/.ssh/aur
```

In the GitHub repository settings, allow Actions to create pull requests and grant the workflow read/write access. The first successful AUR push creates the `breakd` package base; later releases update it. The package can then be installed with:

```bash
yay -S breakd
systemctl --user enable --now breakd.service
```

The local release helpers require a committed `vX.Y.Z` tag:

```bash
scripts/build-source-archive.sh v0.1.0
scripts/build-arch-package.sh v0.1.0 dist/breakd-0.1.0.tar.gz
```
