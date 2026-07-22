// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows OS credential backend: generic credentials in Credential Manager,
//! which encrypts the blob with the user's DPAPI key.
//!
//! Target names and the UTF-16 blob encoding match what the `keyring` crate
//! wrote before this backend replaced it, so credentials stored by earlier
//! versions still resolve.

use anyhow::{anyhow, Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_ENTERPRISE,
    CRED_TYPE_GENERIC,
};

/// `keyring`'s target-name convention: `keyring:{user}@{service}`.
pub fn target_name(service: &str, username: &str) -> String {
    format!("keyring:{username}@{service}")
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn set(target: &str, plaintext: &str) -> Result<()> {
    let mut target = wide(target);
    let mut blob: Vec<u8> = plaintext
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: u32::try_from(blob.len()).context("secret is too large to store")?,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_ENTERPRISE,
        ..Default::default()
    };
    // SAFETY: every pointer in `credential` refers to a live local buffer, and
    // CredWriteW copies the credential before returning.
    unsafe { CredWriteW(&credential, 0) }.context("storing the credential")
}

pub fn get(target: &str) -> Result<Option<String>> {
    let target = wide(target);
    let mut credential = std::ptr::null_mut();
    // SAFETY: `target` is a live NUL-terminated buffer; on success the service
    // hands back an allocation that is freed via CredFree below.
    let read = unsafe {
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut credential,
        )
    };
    if let Err(error) = read {
        if error.code() == ERROR_NOT_FOUND.to_hresult() {
            return Ok(None);
        }
        return Err(error).context("reading the credential");
    }
    if credential.is_null() {
        return Ok(None);
    }

    // SAFETY: CredReadW reported success, so `credential` points at a valid
    // CREDENTIALW whose blob is `CredentialBlobSize` bytes long.
    let result = unsafe {
        let blob = std::slice::from_raw_parts(
            (*credential).CredentialBlob,
            (*credential).CredentialBlobSize as usize,
        );
        decode_utf16(blob)
    };
    // SAFETY: `credential` came from CredReadW and is freed exactly once.
    unsafe { CredFree(credential.cast()) };
    result.map(Some)
}

fn decode_utf16(blob: &[u8]) -> Result<String> {
    if blob.len() % 2 != 0 {
        return Err(anyhow!("the stored secret is not valid UTF-16"));
    }
    let units: Vec<u16> = blob
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).context("the stored secret is not valid UTF-16")
}

pub fn delete(target: &str) -> Result<()> {
    let target = wide(target);
    // SAFETY: `target` is a live NUL-terminated buffer.
    let deleted = unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) };
    match deleted {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        Err(error) => Err(error).context("deleting the credential"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_match_the_scheme_the_keyring_crate_wrote() {
        assert_eq!(
            target_name("halod", "nanoleaf/token"),
            "keyring:nanoleaf/token@halod"
        );
    }

    #[test]
    fn utf16_blobs_round_trip_through_the_decoder() {
        let blob: Vec<u8> = "héllo 世界"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_utf16(&blob).unwrap(), "héllo 世界");
    }

    #[test]
    fn an_odd_length_blob_is_rejected_rather_than_truncated() {
        assert!(decode_utf16(&[0x41]).is_err());
    }
}
