use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};
use uuid::Uuid;
macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        pub struct $name(Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn parse(value: &str) -> Result<Self, String> {
                if !value.starts_with($prefix) {
                    return Err(format!("invalid {}", stringify!($name)));
                }
                Uuid::parse_str(&value[$prefix.len()..])
                    .map(Self)
                    .map_err(|_| format!("invalid {}", stringify!($name)))
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $prefix, self.0)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = String::deserialize(d)?;
                Self::parse(&value).map_err(serde::de::Error::custom)
            }
        }
        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }
    };
}
id_type!(SessionId, "sess_");
id_type!(TurnId, "turn_");
id_type!(StepId, "step_");
id_type!(ToolCallId, "call_");
id_type!(ExecutionId, "exec_");
id_type!(EventId, "evt_");
id_type!(TaskId, "task_");
