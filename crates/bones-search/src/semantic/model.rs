use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "semantic-ort")]
use ort::session::Session;

const MODEL_FILENAME: &str = "minilm-l6-v2-int8.onnx";

#[cfg(feature = "bundled-model")]
const BUNDLED_MODEL_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/minilm-l6-v2-int8.onnx"
));

/// Wrapper around an ONNX Runtime session for the embedding model.
pub struct SemanticModel {
    #[cfg(feature = "semantic-ort")]
    session: Session,
}

impl SemanticModel {
    /// Load the model from the OS cache directory.
    ///
    /// If the cache file is missing or has a mismatched SHA256, this extracts
    /// bundled model bytes into the cache directory before creating an ORT
    /// session.
    pub fn load() -> Result<Self> {
        let path = Self::model_cache_path()?;

        if !Self::is_cached_valid(&path) {
            Self::extract_to_cache(&path)?;
        }

        #[cfg(feature = "semantic-ort")]
        {
            let session = Session::builder()
                .context("failed to create ONNX Runtime session builder")?
                .commit_from_file(&path)
                .with_context(|| {
                    format!("failed to load semantic model from {}", path.display())
                })?;
            return Ok(Self { session });
        }

        #[cfg(not(feature = "semantic-ort"))]
        {
            let _ = path;
            bail!("semantic runtime unavailable: compile bones-search with `semantic-ort`");
        }
    }

    /// Return the OS-appropriate cache path for the model file.
    ///
    /// Uses `dirs::cache_dir() / bones / models / minilm-l6-v2-int8.onnx`.
    pub fn model_cache_path() -> Result<PathBuf> {
        let mut path = dirs::cache_dir().context("unable to determine OS cache directory")?;
        path.push("bones");
        path.push("models");
        path.push(MODEL_FILENAME);
        Ok(path)
    }

    /// Check if cached model matches expected SHA256.
    pub fn is_cached_valid(path: &Path) -> bool {
        let Some(expected_sha256) = expected_model_sha256() else {
            return false;
        };

        let Ok(contents) = fs::read(path) else {
            return false;
        };

        sha256_hex(&contents) == expected_sha256
    }

    /// Extract bundled model bytes to cache directory.
    pub fn extract_to_cache(path: &Path) -> Result<()> {
        let bundled = bundled_model_bytes().ok_or_else(|| {
            anyhow!(
                "semantic model bytes are not bundled; enable `bundled-model` with a packaged ONNX file"
            )
        })?;

        let parent = path.parent().with_context(|| {
            format!(
                "model cache path '{}' has no parent directory",
                path.display()
            )
        })?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create semantic model cache directory {}",
                parent.display()
            )
        })?;

        let temp_path = parent.join(format!("{MODEL_FILENAME}.tmp"));
        fs::write(&temp_path, bundled)
            .with_context(|| format!("failed to write bundled model to {}", temp_path.display()))?;

        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to replace existing model {}", path.display()))?;
        }

        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "failed to move extracted model from {} to {}",
                temp_path.display(),
                path.display()
            )
        })?;

        if !Self::is_cached_valid(path) {
            bail!(
                "extracted semantic model at {} failed SHA256 verification",
                path.display()
            );
        }

        Ok(())
    }

    /// Run inference for a single text input.
    pub fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        #[cfg(feature = "semantic-ort")]
        {
            let _ = &self.session;
            bail!("embedding inference is not implemented yet");
        }

        #[cfg(not(feature = "semantic-ort"))]
        {
            let _ = self;
            bail!("semantic runtime unavailable: compile bones-search with `semantic-ort`");
        }
    }

    /// Batch inference for efficiency.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|text| self.embed(text)).collect()
    }
}

/// Check if semantic search is currently available.
#[must_use]
pub fn is_semantic_available() -> bool {
    SemanticModel::load().is_ok()
}

fn bundled_model_bytes() -> Option<&'static [u8]> {
    #[cfg(feature = "bundled-model")]
    {
        if BUNDLED_MODEL_BYTES.is_empty() {
            return None;
        }

        return Some(BUNDLED_MODEL_BYTES);
    }

    #[cfg(not(feature = "bundled-model"))]
    {
        None
    }
}

fn expected_model_sha256() -> Option<String> {
    bundled_model_bytes().map(sha256_hex)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cache_path_uses_expected_suffix() {
        let path = SemanticModel::model_cache_path().expect("cache path should resolve");
        let expected = Path::new("bones")
            .join("models")
            .join("minilm-l6-v2-int8.onnx");
        assert!(path.ends_with(expected));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        let digest = sha256_hex(b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(not(feature = "bundled-model"))]
    #[test]
    fn cached_model_is_invalid_without_embedded_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir must be created");
        let model = tmp.path().join("minilm-l6-v2-int8.onnx");
        fs::write(&model, b"anything").expect("test file should be writable");

        assert!(!SemanticModel::is_cached_valid(&model));
    }

    #[cfg(not(feature = "bundled-model"))]
    #[test]
    fn extract_to_cache_fails_without_bundled_model() {
        let tmp = tempfile::tempdir().expect("tempdir must be created");
        let model = tmp.path().join("minilm-l6-v2-int8.onnx");

        let err =
            SemanticModel::extract_to_cache(&model).expect_err("should fail without bundled model");
        assert!(err.to_string().contains("not bundled"));
    }

    #[cfg(not(feature = "semantic-ort"))]
    #[test]
    fn semantic_is_reported_unavailable_without_runtime_feature() {
        assert!(!is_semantic_available());
    }
}
