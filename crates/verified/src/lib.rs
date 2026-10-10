//! The `verified` library.

/// Returns the `verified` package version.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_version_is_available() {
        assert!(
            !super::version().is_empty(),
            "the package version should be set by Cargo"
        );
    }
}
