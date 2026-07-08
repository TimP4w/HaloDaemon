//! HID transport and its discovery/hotplug service.
//!
//! - [`transport`] — byte movement only (`HidTransport`, framing, routing hook).
//! - [`discovery`] — enumeration, the hotplug loop, and wired/wireless adoption
//!   policy. Registered as a `TransportScanner`; the hotplug loop is driven from
//!   `main` via [`hotplug_monitor`].

mod discovery;
mod transport;

pub use discovery::hotplug_monitor;
pub use transport::HidTransport;
