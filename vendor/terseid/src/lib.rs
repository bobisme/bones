#![forbid(unsafe_code)]

pub mod children;
pub mod config;
pub mod error;
pub mod generate;
pub mod hash;
pub mod parse;
pub mod resolve;

pub use children::{child_id, id_depth, is_child_id};
pub use config::IdConfig;
pub use error::{Result, TerseIdError};
pub use generate::IdGenerator;
pub use parse::{ParsedId, is_valid_id_format, normalize_id, parse_id, validate_prefix};
pub use resolve::{IdResolver, MatchType, ResolvedId, ResolverConfig, find_matching_ids};

/// Compute a base36 hash of the input, truncated or zero-padded to `length` characters.
pub fn hash(input: impl AsRef<[u8]>, length: usize) -> String {
    hash::hash(input, length)
}
