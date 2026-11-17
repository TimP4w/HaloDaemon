// SPDX-License-Identifier: GPL-3.0-or-later
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginApi {
    pub major: u32,
    pub minor: u32,
}

pub const HOST_PLUGIN_API: PluginApi = PluginApi { major: 3, minor: 0 };

impl PluginApi {
    pub fn is_compatible_with(self, host: PluginApi) -> bool {
        self.major == host.major && self.minor <= host.minor
    }
}

impl fmt::Display for PluginApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl FromStr for PluginApi {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("plugin api '{value}' is not <major>.<minor>");
        let (major, minor) = value.split_once('.').ok_or_else(invalid)?;
        let number = |part: &str| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse().map_err(|_| invalid())
        };
        Ok(Self {
            major: number(major)?,
            minor: number(minor)?,
        })
    }
}

impl Serialize for PluginApi {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PluginApi {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(major: u32, minor: u32) -> PluginApi {
        PluginApi { major, minor }
    }

    #[test]
    fn compatible_means_same_major_and_no_newer_minor() {
        let host = api(3, 2);
        assert!(api(3, 0).is_compatible_with(host));
        assert!(api(3, 2).is_compatible_with(host));
        assert!(!api(3, 3).is_compatible_with(host));
        assert!(!api(2, 0).is_compatible_with(host));
        assert!(!api(4, 0).is_compatible_with(host));
    }

    #[test]
    fn parse_and_display_round_trip() {
        for value in ["0.0", "3.0", "3.12", "10.4"] {
            assert_eq!(value.parse::<PluginApi>().unwrap().to_string(), value);
        }
    }

    #[test]
    fn parse_rejects_anything_but_two_numbers() {
        for value in ["3", "3.", ".1", "3.0.1", "v3.0", "3.x", "-1.0", "3 .0", ""] {
            assert!(value.parse::<PluginApi>().is_err(), "{value}");
        }
    }

    #[test]
    fn serializes_as_a_quoted_string() {
        let yaml = serde_yaml::to_string(&api(3, 1)).unwrap();
        assert_eq!(yaml.trim(), "'3.1'");
        assert_eq!(
            serde_yaml::from_str::<PluginApi>("'3.1'").unwrap(),
            api(3, 1)
        );
    }
}
