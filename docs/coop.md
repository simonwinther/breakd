# Co-op deployment and protocol

Co-op deliberately has two small pieces:

- the existing `breakd` daemon is either a host or a guest;
- `breakd-relay` authenticates room connections and forwards bounded JSON
  WebSocket messages.

The relay never runs a schedule and never decides whether an action is allowed.
It accepts one host per room, forwards snapshots only from that host, and
forwards action requests only from guests. The host's normal scheduler validates
every action. A room disappears when its final connection closes, and its cached
snapshot is cleared as soon as its host disconnects.

## Run a relay

For a local test:

```bash
cargo run -p breakd-relay -- --listen 127.0.0.1:8787
breakd coop host --relay ws://127.0.0.1:8787/ws
```

For a private Tailscale room, listen on the host's Tailscale interface (or on
`0.0.0.0` with a firewall rule restricted to `tailscale0`) and use the host's
MagicDNS name in the relay URL:

```bash
breakd-relay --listen 0.0.0.0:8787
breakd coop host --relay ws://my-host.my-tailnet.ts.net:8787/ws
```

Prefer the MagicDNS name over a copied `100.x.y.z` address when a machine is
shared between tailnets. Tailscale can present that same machine under different
IPv4 addresses to its owner and to a shared user, while the MagicDNS name maps to
the correct address for each person. The invite is a `breakd` CLI value, not a
web page: the guest must run `breakd coop join '<complete invite>'` rather than
opening it in a browser.

Plain `ws://` exposes the room token to the network. Use it only on localhost or
another trusted, encrypted network. For internet use, keep the process bound to
localhost and put a TLS reverse proxy in front of it. For example, a Caddy site
can proxy WebSockets without relay-specific headers:

```caddyfile
breaks.example.net {
  reverse_proxy 127.0.0.1:8787
}
```

Then use `wss://breaks.example.net/ws` as the relay URL. The relay accepts any
request path, so a proxy can dedicate `/ws` or an entire hostname to it.

On NixOS, the flake module can run the isolated relay service:

```nix
{
  services.breakd-relay = {
    enable = true;
    listen = "127.0.0.1:8787";
    maxRoomSize = 8;
    maxRooms = 256;
  };
}
```

The service uses a dynamic user and filesystem hardening. It intentionally does
not open a firewall port or terminate TLS.

## Room lifecycle

The host creates a random UUID room token and persists its mode, relay
URL, and token in `~/.config/breakd/config.toml`, which breakd writes with mode
`0600`. The printed invite has this form:

```text
wss://breaks.example.net/ws#breakd=0123456789abcdef0123456789abcdef
```

URL fragments are not included in HTTP or WebSocket requests. The joining daemon
separates the fragment and sends the token as `Authorization: Bearer ...` during
the WebSocket upgrade. Reverse-proxy access logs therefore do not normally
contain the room token. Avoid putting full invites in shell history, screenshots,
or public chat.

Run `breakd coop host` again to make a new token and invalidate the previous room
from that host. `breakd coop leave` clears the relay URL and token and resets a
fresh local schedule.

`breakd coop host` waits until the host is connected before printing its final
status. `breakd coop join` waits for the first authoritative host snapshot, so a
successful return means `connected`, `host_present`, and `following_host` are all
true. A timeout only stops the CLI wait; the daemon keeps reconnecting in the
background and `breakd coop status` shows its latest state and error.

## Synchronization and failure behavior

The host publishes at most one regular snapshot per second and immediately after
local or guest-requested actions. A working snapshot includes the next break's
absolute Unix start time, duration, type, stable due ID, and strict/manual-resume
policy. That lets both schedulers start the same session locally without waiting
for a round trip at the deadline. Active-break and pause snapshots let a guest
join midway through a room.

Guests use their own display, content, sound, and monitor configuration. They do
not run idle, lock, or suspend transitions while following the host. Native
WebSocket ping frames keep the one connection alive, and reconnects use bounded
exponential backoff. The client rejects stale revisions and messages larger than
128 KiB.

When the configured `coop.disconnect_grace` elapses without a snapshot (10
seconds by default), a guest discards the mirrored state and begins a fresh local
schedule. This avoids leaving the user with a frozen or overdue remote break.
If the host later returns, the next valid snapshot becomes authoritative again.

Absolute deadlines assume both computers have ordinary NTP-style clock
synchronization. Network and scheduler tick jitter mean the overlays are not a
hard real-time barrier, but under normal clocks they begin within the local
250 ms scheduler tick rather than one relay round trip apart.

## Resource model

When co-op is off, no network task or connection is created. Enabling it adds one
Tokio task, one WebSocket, bounded event and action buffers, and a latest-value
snapshot channel. The standalone relay uses one task and small outbound queue per
connection, stores only one snapshot per live room, and caps rooms at 8
connections by default (configurable from 2 to 64). It has no GTK, Wayland,
database, or account-system dependency. Guests cannot create empty rooms, and
the relay also caps simultaneously live rooms at 256 by default.
