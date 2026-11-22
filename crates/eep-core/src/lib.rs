//! eep-core — the formats and building blocks shared by the `eep` client and
//! the `eep-build` server tool.
//!
//! Nothing here talks to the Nix daemon. The client drives Nix through its
//! CLI; this crate only knows the *data*: store paths, narinfo files, the
//! binary cache's `.ls` listings, and eep's own index / card / catalog.

pub mod card;
pub mod catalog;
pub mod desktop;
pub mod http;
pub mod index;
pub mod listing;
pub mod narinfo;
pub mod nixbase32;
pub mod par;
pub mod patches;
pub mod store_path;
pub mod time;

pub use store_path::StorePath;

/// Where the cache we take prebuilt pieces from lives.
pub const UPSTREAM_CACHE: &str = "https://cache.nixos.org";

/// App ids: lower-case ASCII letters, digits and `-`, 1–64 chars, starting
/// with a letter. They name files (`roots/<id>`, `eep-<id>.desktop`), so
/// they must never contain a path separator.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::valid_id;

    #[test]
    fn ids() {
        assert!(valid_id("firefox"));
        assert!(valid_id("vulkan-tools"));
        assert!(!valid_id(""));
        assert!(!valid_id("Firefox"));
        assert!(!valid_id("1password"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("a/b"));
    }
}
