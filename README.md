# breakd

Break reminders for Hyprland on Wayland.

## Install

Install the AUR package and start the user service:

```bash
yay -S breakd
systemctl --user enable --now breakd.service
breakd status
```

`breakd` runs as your user. It does not need root access or access to `/dev/input`.

## Configure

`breakd` works without a configuration file. To change the defaults:

```bash
mkdir -p ~/.config/breakd
breakd example-config > ~/.config/breakd/config.toml
$EDITOR ~/.config/breakd/config.toml
breakd reload
```

The default schedule is a 20-second mini break every 10 minutes and a 5-minute long break every 30 minutes. Durations accept values such as `20s`, `10m`, and `1h`.

Most changes take effect after `breakd reload`. Restart the service after changing `[idle]` or `[logging]`:

```bash
systemctl --user restart breakd.service
```

### Strict mode

Set `strict.mode` to one of these values:

- `off`: skip and postpone are available immediately.
- `delay`: controls unlock after `strict.minimum_visible`.
- `entire`: the break cannot be skipped.

### Pointer and keyboard input

`display.pointer_mode` controls where clicks go during a break:

- `block`: captures clicks across the full overlay. Controls remain clickable. This is the default.
- `controls`: captures clicks on the content panel and passes background clicks through.
- `none`: makes the full overlay click-through.

`display.keyboard_mode` accepts `none`, `on-demand`, or `exclusive`. The default is `on-demand`.

## Monitors

Run `breakd outputs` to list connected monitors and their stable identifiers:

```text
edid:<make>:<model>:<serial>
connector:<name>
```

Use one of these values for `display.mode`:

- `all`: show the full break on every monitor.
- `focused`: show it on the focused monitor.
- `cursor`: show it on the monitor containing the cursor.
- `primary`: use `display.primary_monitor`.
- `configured`: use `display.preferred_monitor`.
- `dim-all-content-one`: dim every monitor and put the message and controls on the monitor selected by `display.content_selector`.

For example:

```toml
[display]
mode = "configured"
preferred_monitor = "connector:DP-1"
fallback = ["focused", "cursor", "primary"]
pointer_mode = "block"
keyboard_mode = "on-demand"
opacity = 0.88
```

If a configured monitor is unavailable, `breakd` follows the entries in `display.fallback`.

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

`skip` and `postpone` follow the active strict-mode and postpone settings.

## Hyprland bindings

Any Hyprland binding can call the CLI. For Hyprland 0.55+ Lua configuration:

```lua
hl.bind("SUPER + SHIFT + B", hl.dsp.exec_cmd("breakd toggle"))
hl.bind("SUPER + SHIFT + S", hl.dsp.exec_cmd("breakd skip"))
hl.bind("SUPER + SHIFT + P", hl.dsp.exec_cmd("breakd postpone"))
hl.bind("SUPER + SHIFT + M", hl.dsp.exec_cmd("breakd mini"))
hl.bind("SUPER + SHIFT + L", hl.dsp.exec_cmd("breakd long"))
```

An optional layer rule disables overlay animations:

```lua
hl.layer_rule({
  match = { namespace = "^breakd-overlay$" },
  no_anim = true,
})
```

Reload Hyprland after editing its configuration:

```bash
hyprctl reload
hyprctl configerrors
```

## Troubleshooting

Check the daemon, desktop integrations, and monitor detection:

```bash
breakd doctor
breakd outputs
systemctl --user status breakd.service
journalctl --user -u breakd.service -f
```

If the service cannot see the Wayland or Hyprland environment, import the session variables and restart it:

```bash
systemctl --user import-environment \
  WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE DBUS_SESSION_BUS_ADDRESS
systemctl --user restart breakd.service
```

## Build from source

Install the build dependencies:

```bash
sudo pacman -S --needed base-devel gtk4 gtk4-layer-shell rust
cargo build --locked --release
```

Install the binary and user service:

```bash
install -Dm755 target/release/breakd ~/.local/bin/breakd
install -Dm644 packaging/systemd/breakd-local.service \
  ~/.config/systemd/user/breakd.service
install -Dm600 config.example.toml ~/.config/breakd/config.toml
systemctl --user daemon-reload
systemctl --user enable --now breakd.service
```
