pub mod control_hub;
pub mod f_fan;
pub mod kraken;

/// Vendor-local alias so NZXT device code can keep using `NzxtFanHub` without
/// pulling in the generic name from the top-level capability layer.
pub use crate::drivers::FanHub as NzxtFanHub;
