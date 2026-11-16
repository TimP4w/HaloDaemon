// SPDX-License-Identifier: GPL-3.0-or-later
mod api;
pub mod catalog;
pub mod fetch;
pub mod package;

pub use api::{PluginApi, HOST_PLUGIN_API};
pub use catalog::{open_catalog, Catalog, CatalogError, CatalogPackage, RepositorySigningKey};

use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex_lower(Sha256::digest(bytes))
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
