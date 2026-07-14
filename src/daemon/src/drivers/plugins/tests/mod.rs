// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic plugin-machinery tests (loader/manifest/permission/repo), against
//! synthetic fixtures. Vendor-specific equivalence tests moved to the
//! official plugin repo's own `halod plugin-test` CI run.

mod registry;

#[test]
fn declared_write_rate_limit_preserves_manifest_value() {
    let limit = super::declared_write_rate_limit(Some(12_345)).expect("declared limit");
    assert_eq!(limit.max_bytes_per_sec, 12_345);
    assert!(super::declared_write_rate_limit(None).is_none());
}
