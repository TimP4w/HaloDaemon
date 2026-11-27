// SPDX-License-Identifier: GPL-3.0-or-later
//! HID byte transport; discovery orchestration lives in `application::observers::hid`.

mod transport;

pub use transport::HidTransport;

/// The part of an enumeration path shared by every collection of one USB
/// interface. Windows paths name the collection and its instance index
/// (`\\?\HID#VID_046D&PID_C547&MI_02&Col01#d&28800fc7&0&0001#{guid}`), which
/// this drops; other platforms enumerate one node per interface.
pub(crate) fn physical_path_stem(path: &str) -> String {
    let mut segments = path.split('#');
    let (Some(_), Some(hardware), Some(instance)) =
        (segments.next(), segments.next(), segments.next())
    else {
        return path.to_owned();
    };
    let is_collection = |token: &&str| {
        token
            .get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case("col"))
    };
    let mut hardware_tokens: Vec<&str> = hardware.split('&').collect();
    let token_count = hardware_tokens.len();
    hardware_tokens.retain(|t| !is_collection(t));
    let mut instance_tokens: Vec<&str> = instance.split('&').collect();
    if hardware_tokens.len() < token_count && instance_tokens.len() > 1 {
        instance_tokens.pop();
    }
    format!(
        "{}#{}",
        hardware_tokens.join("&"),
        instance_tokens.join("&")
    )
}

#[cfg(test)]
mod tests {
    use super::physical_path_stem;

    const SHORT: &str = "\\\\?\\HID#VID_046D&PID_C547&MI_02&Col01#b&3090b098&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const LONG: &str = "\\\\?\\HID#VID_046D&PID_C547&MI_02&Col02#b&3090b098&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const TWIN: &str = "\\\\?\\HID#VID_046D&PID_C547&MI_02&Col01#d&28800fc7&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
    const KEYBOARD: &str = "\\\\?\\HID#VID_046D&PID_C547&MI_01&Col01#b&34627d98&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}\\KBD";

    #[test]
    fn stem_joins_collections_of_one_interface_and_splits_twins() {
        assert_eq!(physical_path_stem(SHORT), physical_path_stem(LONG));
        assert_ne!(physical_path_stem(SHORT), physical_path_stem(TWIN));
        assert_ne!(physical_path_stem(SHORT), physical_path_stem(KEYBOARD));
    }

    #[test]
    fn stem_keeps_single_node_paths() {
        assert_eq!(physical_path_stem("/dev/hidraw3"), "/dev/hidraw3");
        assert_eq!(
            physical_path_stem("\\\\?\\HID#VID_046D&PID_C547&MI_00#d&189be0a7&0&0000#{guid}"),
            "VID_046D&PID_C547&MI_00#d&189be0a7&0&0000"
        );
    }
}
