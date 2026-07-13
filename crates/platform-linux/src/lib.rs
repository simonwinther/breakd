mod clock;
mod hyprland;
mod idle;
mod logind;
mod notifications;
mod state_store;
mod wayland_probe;

pub use clock::{ClockError, LinuxClock};
pub use hyprland::{HyprlandClient, HyprlandError};
pub use idle::{IdleCapability, IdleEvent, spawn_idle_monitor};
pub use logind::{PowerEvent, spawn_logind_monitor};
pub use notifications::NotificationClient;
pub use state_store::{StateStore, StateStoreError};
pub use wayland_probe::{WaylandGlobal, WaylandProbeError, probe_wayland_globals};
