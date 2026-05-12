//! Identifier types. `ULID` for sessions and agents — lexicographic order ≈
//! time order, no central counter, 26-char base32 representation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! ulid_newtype {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            /// Generate a fresh id from the current time + cryptographic randomness.
            pub fn new() -> Self {
                Self(Ulid::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_str(s).map(Self)
            }
        }
    };
}

ulid_newtype!(SessionId);
ulid_newtype!(AgentId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_session_id() {
        let id = SessionId::new();
        let parsed: SessionId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn roundtrip_agent_id_via_json() {
        let id = AgentId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: AgentId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn new_ids_differ() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }
}
