use std::path::Path;

use serde::Deserialize;

use crate::{
    builds::{self, Build},
    index_signature,
};

// Existing Sunrise mod tool's sdk_fetch::SMALL.
pub const SMALL: u64 = 16 << 20;

#[derive(Deserialize)]
pub struct Catalog {
    schema: u8,
    #[serde(default)]
    pub builds: Vec<Build>,
}

pub fn verify(path: &Path, sig: &Path, key: &Path) -> Result<Catalog, String> {
    let bytes = index_signature::verified_bytes(path, sig, key)?;
    let index: Catalog =
        serde_json::from_slice(&bytes).map_err(|e| format!("verified index: {e}"))?;
    if index.schema != 2 {
        return Err("verified index is not schema 2".into());
    }
    builds::validate(index.builds.iter().map(|build| (&build.id, &build.commit)))?;
    Ok(index)
}

#[cfg(test)]
mod tests;
