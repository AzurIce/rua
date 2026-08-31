use serde::{Deserialize, Serialize};

macro_rules! id_newtype {
    ($name:ident) => {
        /// ULID-backed identifier. Time-ordered; safe to pre-allocate.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub ulid::Ulid);

        impl $name {
            pub fn new() -> Self {
                Self(ulid::Ulid::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Ok(Self(s.parse()?))
            }
        }
    };
}

id_newtype!(NodeId);
id_newtype!(CursorId);
