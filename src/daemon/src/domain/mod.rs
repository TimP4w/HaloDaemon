// SPDX-License-Identifier: GPL-3.0-or-later
//! Business domains and their application-facing behavior.

pub mod cooling;
pub mod device;
pub mod events;
pub mod input;
pub mod lcd;
pub mod lighting;
pub mod plugin;
pub mod profiles;
pub mod registry;

#[cfg(test)]
mod layering_tests {
    const FROZEN_APPLICATION_IMPORTS: usize = 46;

    fn application_imports(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    application_imports(&path)
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    std::fs::read_to_string(&path)
                        .unwrap()
                        .lines()
                        .filter(|line| {
                            let line = line.trim_start();
                            let line = line
                                .strip_prefix("pub(crate) ")
                                .or_else(|| line.strip_prefix("pub(super) "))
                                .or_else(|| line.strip_prefix("pub "))
                                .unwrap_or(line);
                            line.starts_with("use crate::application")
                        })
                        .count()
                } else {
                    0
                }
            })
            .sum()
    }

    #[test]
    fn domain_imports_from_application_do_not_grow() {
        let domain = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
        let count = application_imports(&domain);
        assert!(
            count <= FROZEN_APPLICATION_IMPORTS,
            "domain gained application imports: {count} > {FROZEN_APPLICATION_IMPORTS}"
        );
    }
}
