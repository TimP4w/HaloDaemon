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
    const FROZEN_APPLICATION_IMPORTS: usize = 123;

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
                        .filter(|line| !line.trim_start().starts_with("//"))
                        .map(|line| line.matches("crate::application").count())
                        .sum()
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
