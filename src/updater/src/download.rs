// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::{Read, Write};

use halod_shared::update::UpdateError;

const MAX_REDIRECTS: u32 = 5;
const CHUNK: usize = 64 * 1024;

pub fn agent() -> ureq::Agent {
    halod_plugin_signing::fetch::agent(MAX_REDIRECTS)
}

/// Copies at most `limit` bytes, reporting the running total after each chunk.
pub fn copy_capped(
    mut reader: impl Read,
    mut writer: impl Write,
    limit: u64,
    mut progress: impl FnMut(u64),
) -> Result<u64, UpdateError> {
    let mut buffer = vec![0; CHUNK];
    let mut done = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(|e| {
            log::warn!("reading the download: {e}");
            UpdateError::Network
        })?;
        if read == 0 {
            return Ok(done);
        }
        done += read as u64;
        if done > limit {
            return Err(UpdateError::TooLarge);
        }
        writer.write_all(&buffer[..read]).map_err(|e| {
            log::warn!("writing the download: {e}");
            UpdateError::Io
        })?;
        progress(done);
    }
}

pub struct Download {
    pub reader: Box<dyn Read + Send>,
    pub total: Option<u64>,
}

pub fn open(agent: &ureq::Agent, url: &str) -> Result<Download, UpdateError> {
    let response = agent
        .get(url)
        .header("User-Agent", "HaloDaemon-updater")
        .call()
        .map_err(|e| {
            log::warn!("requesting {url}: {e}");
            UpdateError::Network
        })?;
    let total = response.body().content_length();
    Ok(Download {
        reader: Box::new(response.into_body().into_reader()),
        total,
    })
}

pub fn fetch(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let mut bytes = Vec::new();
    copy_capped(open(agent, url)?.reader, &mut bytes, limit, |_| {})?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_everything_under_the_limit_and_reports_progress() {
        let data = vec![7u8; CHUNK * 2 + 10];
        let mut out = Vec::new();
        let mut reports = Vec::new();
        let copied = copy_capped(data.as_slice(), &mut out, data.len() as u64, |n| {
            reports.push(n)
        })
        .unwrap();
        assert_eq!(copied, data.len() as u64);
        assert_eq!(out, data);
        assert_eq!(reports.last(), Some(&(data.len() as u64)));
        assert!(reports.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn stops_once_the_limit_is_exceeded() {
        let data = vec![1u8; 100];
        let mut out = Vec::new();
        assert_eq!(
            copy_capped(data.as_slice(), &mut out, 99, |_| {}),
            Err(UpdateError::TooLarge)
        );
        assert!(out.len() < data.len());
    }

    struct Failing(std::io::ErrorKind);

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
    }

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn read_and_write_failures_are_told_apart() {
        let broken = Failing(std::io::ErrorKind::ConnectionReset);
        assert_eq!(
            copy_capped(broken, Vec::new(), 10, |_| {}),
            Err(UpdateError::Network)
        );
        let full = Failing(std::io::ErrorKind::StorageFull);
        assert_eq!(
            copy_capped([1u8; 4].as_slice(), full, 10, |_| {}),
            Err(UpdateError::Io)
        );
    }
}
