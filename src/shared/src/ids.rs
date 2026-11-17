// SPDX-License-Identifier: GPL-3.0-or-later
use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId {
    pub kind: &'static str,
    pub value: String,
    pub rule: &'static str,
}

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} '{}' {}", self.kind, self.value, self.rule)
    }
}

impl std::error::Error for InvalidId {}

pub const IDENTIFIER_RULE: &str = "must contain only ASCII letters, digits, '_' or '-'";
pub const DEVICE_ID_RULE: &str = "must be a non-empty path segment without control characters";

#[must_use]
pub fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn is_device_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

/// Replaces every character a device id may not contain so a serial read from
/// hardware can serve as an id suffix.
pub fn device_id_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

macro_rules! string_id {
    ($name:ident, $kind:literal, $valid:ident, $rule:ident) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }

            fn checked(value: String) -> Result<Self, InvalidId> {
                if $valid(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidId {
                        kind: $kind,
                        value,
                        rule: $rule,
                    })
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::checked(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::checked(value.to_owned())
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::checked(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl From<&$name> for $name {
            fn from(value: &$name) -> Self {
                value.clone()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        impl PartialEq<$name> for &$name {
            fn eq(&self, other: &$name) -> bool {
                **self == *other
            }
        }

        impl PartialEq<&$name> for $name {
            fn eq(&self, other: &&$name) -> bool {
                *self == **other
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

string_id!(DeviceId, "device id", is_device_id, DEVICE_ID_RULE);

impl DeviceId {
    pub fn sanitized(value: &str) -> Self {
        let safe = device_id_safe(value);
        if is_device_id(&safe) {
            Self(safe)
        } else {
            Self("_".to_owned())
        }
    }
}
string_id!(PluginId, "plugin id", is_identifier, IDENTIFIER_RULE);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    proptest! {
        #[test]
        fn display_and_from_str_round_trip(value in "[A-Za-z0-9_-]{1,40}") {
            let device: DeviceId = value.parse().unwrap();
            prop_assert_eq!(device.to_string(), value.clone());
            let plugin: PluginId = value.parse().unwrap();
            prop_assert_eq!(plugin.to_string().parse::<PluginId>().unwrap(), plugin);
        }

        #[test]
        fn serde_is_the_plain_string(value in "[A-Za-z0-9_-]{1,40}") {
            let id: DeviceId = value.parse().unwrap();
            let json = serde_json::to_string(&id).unwrap();
            prop_assert_eq!(&json, &serde_json::to_string(&value).unwrap());
            prop_assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), id);
        }

        #[test]
        fn a_device_id_accepts_any_serial_once_made_safe(serial in "\\PC{0,24}") {
            let safe = device_id_safe(&serial);
            prop_assert_eq!(safe.len(), serial.len());
            if !safe.is_empty() && safe != "." && safe != ".." {
                prop_assert!(safe.parse::<DeviceId>().is_ok(), "{safe}");
            }
        }

        #[test]
        fn typed_and_string_keyed_maps_agree(
            entries in proptest::collection::vec(("[a-z0-9_-]{1,12}", 0u32..100), 0..30),
            probes in proptest::collection::vec("[a-z0-9_-]{1,12}", 0..30),
        ) {
            let mut typed: HashMap<DeviceId, u32> = HashMap::new();
            let mut plain: HashMap<String, u32> = HashMap::new();
            for (key, value) in &entries {
                typed.insert(key.parse().unwrap(), *value);
                plain.insert(key.clone(), *value);
            }
            prop_assert_eq!(typed.len(), plain.len());
            for probe in entries.iter().map(|(key, _)| key).chain(&probes) {
                prop_assert_eq!(typed.get(probe.as_str()), plain.get(probe));
            }
        }
    }

    #[test]
    fn every_way_in_rejects_what_the_rule_forbids() {
        for bad in ["../x", "", "a/b", "a\\b", "a\nb", ".", ".."] {
            assert!(bad.parse::<DeviceId>().is_err(), "{bad}");
            assert!(bad.parse::<PluginId>().is_err(), "{bad}");
            assert!(DeviceId::try_from(bad.to_owned()).is_err(), "{bad}");
            assert!(
                serde_json::from_str::<PluginId>(&format!("{bad:?}")).is_err(),
                "{bad}"
            );
        }
        for plugin_only in ["a.b", "a b", "ä", "A1:B2"] {
            assert!(plugin_only.parse::<DeviceId>().is_ok(), "{plugin_only}");
            assert!(plugin_only.parse::<PluginId>().is_err(), "{plugin_only}");
        }
        assert_eq!(
            "../x".parse::<PluginId>().unwrap_err().to_string(),
            "plugin id '../x' must contain only ASCII letters, digits, '_' or '-'"
        );
    }
}
