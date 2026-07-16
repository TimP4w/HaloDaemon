// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared, lazily loaded access to fonts installed on the host.

use std::collections::BTreeSet;
use std::sync::OnceLock;

fn database() -> &'static fontdb::Database {
    static DATABASE: OnceLock<fontdb::Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        database
    })
}

/// Sorted, deduplicated family names exposed by the host font database.
pub fn families() -> &'static [String] {
    static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    FAMILIES.get_or_init(|| {
        database()
            .faces()
            .filter_map(|face| face.families.first().map(|(family, _)| family.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    })
}

/// Font bytes and collection index for the best match to `family`.
pub fn data(family: &str) -> Option<(Vec<u8>, u32)> {
    let families = [fontdb::Family::Name(family)];
    let id = database().query(&fontdb::Query {
        families: &families,
        ..Default::default()
    })?;
    database().with_face_data(id, |bytes, index| (bytes.to_vec(), index))
}
