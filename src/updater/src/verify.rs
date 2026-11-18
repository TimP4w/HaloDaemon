// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs::File;
use std::io::Read;

use anyhow::Result;
use sha2::{Digest, Sha256};

pub fn hash_file(mut file: &File) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(halod_plugin_signing::hex_lower(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_in_memory_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("installer.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let hash = hash_file(&File::open(&path).unwrap()).unwrap();
        assert_eq!(hash, halod_plugin_signing::sha256_hex(&data));
    }
}
