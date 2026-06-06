//! Logitech HID++ 2.0 devices: the generic feature-driven device, the Unifying/
//! Lightspeed receiver that hosts wireless children, the G560 speaker, and the
//! per-key RGB frame encoder shared by keyboards.

pub mod device;
pub mod g560;
pub mod init;
pub mod key_remap;
pub mod led_positions;
pub mod onboard;
pub mod pk_frame;
pub mod profile;
pub mod receiver;
pub mod rgb;
pub mod state;
