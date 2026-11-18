// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub version: Version,
    pub relaunch: PathBuf,
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Args> {
    let mut version = None;
    let mut relaunch = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| anyhow!("{arg} requires a value"));
        match arg.as_str() {
            "--version" => {
                let text = value()?;
                version = Some(Version::parse(&text).with_context(|| format!("parsing '{text}'"))?);
            }
            "--relaunch" => relaunch = Some(PathBuf::from(value()?)),
            _ => bail!("unexpected argument '{arg}'"),
        }
    }
    Ok(Args {
        version: version.ok_or_else(|| anyhow!("missing --version"))?,
        relaunch: relaunch.ok_or_else(|| anyhow!("missing --relaunch"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args> {
        parse(list.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn parses_both_options_in_any_order() {
        let expected = Args {
            version: Version::new(0, 14, 0),
            relaunch: PathBuf::from(r"C:\halod\halod-gui.exe"),
        };
        assert_eq!(
            args(&[
                "--version",
                "0.14.0",
                "--relaunch",
                r"C:\halod\halod-gui.exe"
            ])
            .unwrap(),
            expected
        );
        assert_eq!(
            args(&[
                "--relaunch",
                r"C:\halod\halod-gui.exe",
                "--version",
                "0.14.0"
            ])
            .unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_missing_malformed_and_unknown_arguments() {
        for bad in [
            &["--version", "0.14.0"][..],
            &["--relaunch", "gui.exe"][..],
            &["--version", "x", "--relaunch", "gui.exe"][..],
            &["--version"][..],
            &["--version", "0.14.0", "--relaunch", "gui.exe", "--url", "x"][..],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }
}
