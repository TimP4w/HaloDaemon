// SPDX-License-Identifier: GPL-3.0-or-later
//! Whether the generated udev rules need the user to do something.

use halod_shared::types::UdevRulesStatus;

pub fn udev_rules_need_action(status: &UdevRulesStatus) -> bool {
    status.supported && !status.current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_needed_only_when_supported_and_not_current() {
        let stale = UdevRulesStatus {
            supported: true,
            current: false,
            installed_path: Some("/etc/udev/rules.d/60-halod.rules".into()),
            ..UdevRulesStatus::default()
        };
        assert!(udev_rules_need_action(&stale));

        let missing = UdevRulesStatus {
            supported: true,
            current: false,
            ..UdevRulesStatus::default()
        };
        assert!(udev_rules_need_action(&missing));

        let current = UdevRulesStatus {
            supported: true,
            current: true,
            ..UdevRulesStatus::default()
        };
        assert!(!udev_rules_need_action(&current));

        assert!(!udev_rules_need_action(&UdevRulesStatus::default()));
    }
}
