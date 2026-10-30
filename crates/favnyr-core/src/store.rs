//! Shared plumbing for the TOML documents Favnyr keeps beside the user's data.
//!
//! Every store answers the same two questions — "load it without ever
//! preventing startup" and "write it without ever truncating the previous
//! copy" — so the answers live here once instead of in six files.
//!
//! The *fallible* loaders (config, workspace) are deliberately NOT part of
//! this module: they report a parse error to the caller instead of silently
//! starting from an empty document, which is a different contract, not a
//! duplicated one.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, Result};

/// A TOML document that must never stop the application from opening.
pub(crate) trait TomlStore: DeserializeOwned + Default {
    /// Message logged when the file exists but cannot be parsed. Kept per
    /// store so a log reader can tell which document was rejected.
    const LABEL: &'static str;

    /// Re-keys whatever an older version of this file stored differently.
    /// Most stores have nothing to do.
    fn after_load(&mut self) {}
}

/// Loads `path`, falling back to an empty store on ANY failure.
///
/// A missing file is the ordinary first-run case. A file that exists but
/// cannot be read back is set aside first (`paths::preserve_unreadable`) so
/// the empty store returned here can never be written over the user's only
/// copy — that is the whole reason this is not just `unwrap_or_default()`.
pub(crate) fn load<T: TomlStore>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(content) => match toml::from_str::<T>(&content) {
            Ok(mut store) => {
                store.after_load();
                store
            }
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "{}", T::LABEL);
                crate::paths::preserve_unreadable(path);
                T::default()
            }
        },
        Err(_) => T::default(),
    }
}

/// Writes `value` as TOML at `path`, creating the parent folder first.
///
/// `map_error` lets each store keep its own [`Error`] variant and wording;
/// everything else — the parent folder, the pretty TOML, the atomic write —
/// is the same everywhere.
pub(crate) fn save<T: Serialize>(
    path: &Path,
    value: &T,
    map_error: impl FnOnce(toml::ser::Error) -> Error,
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(value).map_err(map_error)?;
    crate::paths::write_atomic(path, &text)?;
    Ok(())
}
