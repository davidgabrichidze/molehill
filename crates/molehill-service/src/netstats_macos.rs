//! Traffic counters of a network interface.

/// (received, sent) bytes of the interface called `name`. Not implemented on
/// macOS yet, so OpenConnect tunnels show no traffic figures.
pub fn interface_bytes(_name: &str) -> Option<(u64, u64)> {
    None
}
