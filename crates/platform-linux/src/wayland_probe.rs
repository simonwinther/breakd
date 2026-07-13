use serde::Serialize;
use thiserror::Error;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaylandGlobal {
    pub interface: String,
    pub version: u32,
}

#[derive(Debug, Error)]
pub enum WaylandProbeError {
    #[error("failed to connect to the Wayland compositor: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("failed to receive the Wayland registry: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
}

pub fn probe_wayland_globals() -> Result<Vec<WaylandGlobal>, WaylandProbeError> {
    let connection = Connection::connect_to_env()?;
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    connection.display().get_registry(&queue_handle, ());
    let mut state = ProbeState::default();
    event_queue.roundtrip(&mut state)?;
    state
        .globals
        .sort_by(|left, right| left.interface.cmp(&right.interface));
    Ok(state.globals)
}

#[derive(Default)]
struct ProbeState {
    globals: Vec<WaylandGlobal>,
}

impl Dispatch<WlRegistry, ()> for ProbeState {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            interface, version, ..
        } = event
        {
            state.globals.push(WaylandGlobal { interface, version });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_serializes_with_interface_and_version() {
        let global = WaylandGlobal {
            interface: "wlr_layer_shell_v1".into(),
            version: 5,
        };
        assert_eq!(
            serde_json::to_value(global).unwrap(),
            serde_json::json!({"interface": "wlr_layer_shell_v1", "version": 5})
        );
    }
}
