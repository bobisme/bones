//! `Model2Vec` embedding backend.
//!
//! `Model2Vec` converts transformer models into static token embedding lookup
//! tables at distillation time.  At inference time, embedding a text is just:
//!
//!   tokenize → table lookup → mean pool → L2 normalize
//!
//! No neural-network forward pass, no ONNX runtime.  Dependencies are
//! `safetensors` (to read the weight matrix) and `tokenizers` (`HuggingFace`
//! BPE/WordPiece tokenizer).

#[cfg(feature = "semantic-model2vec")]
use anyhow::{Context, Result, anyhow, bail};
#[cfg(feature = "semantic-model2vec")]
use std::path::Path;
#[cfg(feature = "semantic-model2vec")]
use tokenizers::Tokenizer;

#[cfg(feature = "semantic-model2vec")]
const MODEL_FILENAME: &str = "potion-base-8M.safetensors";
#[cfg(feature = "semantic-model2vec")]
const TOKENIZER_FILENAME: &str = "potion-base-8M-tokenizer.json";
#[cfg(feature = "semantic-model2vec")]
const MODEL_DOWNLOAD_URL_DEFAULT: &str =
    "https://huggingface.co/minishlab/potion-base-8M/resolve/main/model.safetensors";
#[cfg(feature = "semantic-model2vec")]
const TOKENIZER_DOWNLOAD_URL_DEFAULT: &str =
    "https://huggingface.co/minishlab/potion-base-8M/resolve/main/tokenizer.json";

/// Known tensor names that model2vec safetensors files use for the embedding
/// matrix.
#[cfg(feature = "semantic-model2vec")]
const TENSOR_NAMES: &[&str] = &["embeddings", "embedding", "word_embeddings", "embed", "emb"];

/// Static token-embedding model loaded from safetensors.
#[cfg(feature = "semantic-model2vec")]
pub struct Model2VecBackend {
    tokenizer: Tokenizer,
    /// Flat row-major `[vocab_size, dimensions]` embedding matrix.
    embeddings: Vec<f32>,
    vocab_size: usize,
    dimensions: usize,
}

#[cfg(feature = "semantic-model2vec")]
impl Model2VecBackend {
    /// Load a model2vec model and tokenizer from the cache directory.
    pub fn load() -> Result<Self> {
        let root = super::model::SemanticModel::model_cache_root()?;
        let model_path = root.join(MODEL_FILENAME);
        let tokenizer_path = root.join(TOKENIZER_FILENAME);

        ensure_asset(&model_path, MODEL_DOWNLOAD_URL_DEFAULT, "model2vec model")?;
        ensure_asset(
            &tokenizer_path,
            TOKENIZER_DOWNLOAD_URL_DEFAULT,
            "model2vec tokenizer",
        )?;

        Self::load_from_files(&model_path, &tokenizer_path)
    }

    fn load_from_files(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| {
            anyhow!(
                "failed to load model2vec tokenizer from {}: {e}",
                tokenizer_path.display()
            )
        })?;

        let data = std::fs::read(model_path).with_context(|| {
            format!(
                "failed to read model2vec safetensors from {}",
                model_path.display()
            )
        })?;

        let tensors = safetensors::SafeTensors::deserialize(&data)
            .context("failed to deserialize model2vec safetensors")?;

        let tensor = find_embedding_tensor(&tensors)?;
        let shape = tensor.shape();
        if shape.len() != 2 {
            bail!(
                "model2vec embedding tensor has unexpected rank {}: expected 2",
                shape.len()
            );
        }
        let vocab_size = shape[0];
        let dimensions = shape[1];

        let embeddings: Vec<f32> = tensor
            .data()
            .chunks_exact(4)
            .map(|c| {
                f32::from_le_bytes(
                    c.try_into()
                        .expect("chunks_exact(4) guarantees 4-byte slices"),
                )
            })
            .collect();

        if embeddings.len() != vocab_size * dimensions {
            bail!(
                "model2vec embedding data length mismatch: expected {}x{}={}, got {}",
                vocab_size,
                dimensions,
                vocab_size * dimensions,
                embeddings.len()
            );
        }

        Ok(Self {
            tokenizer,
            embeddings,
            vocab_size,
            dimensions,
        })
    }

    /// The dimensionality of the embedding vectors this model produces.
    pub const fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Embed a single text string.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| anyhow!("model2vec tokenization failed: {e}"))?;

        let token_ids = encoding.get_ids();
        let mut sum = vec![0.0_f32; self.dimensions];
        let mut count = 0_usize;

        for &token_id in token_ids {
            let idx = token_id as usize;
            if idx < self.vocab_size {
                let start = idx * self.dimensions;
                let row = &self.embeddings[start..start + self.dimensions];
                for (s, &r) in sum.iter_mut().zip(row) {
                    *s += r;
                }
                count += 1;
            }
        }

        if count == 0 {
            return Ok(vec![0.0; self.dimensions]);
        }

        #[allow(clippy::cast_precision_loss)]
        let inv = 1.0 / count as f32;
        for s in &mut sum {
            *s *= inv;
        }
        normalize_l2(&mut sum);
        Ok(sum)
    }

    /// Batch-embed multiple texts.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|text| self.embed(text)).collect()
    }
}

#[cfg(feature = "semantic-model2vec")]
fn normalize_l2(values: &mut [f32]) {
    let norm_sq: f32 = values.iter().map(|v| v * v).sum();
    if norm_sq > f32::EPSILON {
        let inv_norm = 1.0 / norm_sq.sqrt();
        for v in values {
            *v *= inv_norm;
        }
    }
}

#[cfg(feature = "semantic-model2vec")]
fn find_embedding_tensor<'a>(
    tensors: &'a safetensors::SafeTensors<'a>,
) -> Result<safetensors::tensor::TensorView<'a>> {
    for &name in TENSOR_NAMES {
        if let Ok(tensor) = tensors.tensor(name) {
            return Ok(tensor);
        }
    }

    // Fall back to the only tensor if there's exactly one.
    let names: Vec<_> = tensors.names().into_iter().collect();
    if names.len() == 1 {
        return tensors
            .tensor(names[0].as_str())
            .context("failed to load single tensor from model2vec safetensors");
    }

    bail!(
        "could not find embedding tensor in model2vec safetensors; \
         tried {TENSOR_NAMES:?}, found tensors: {names:?}",
    );
}

#[cfg(feature = "semantic-model2vec")]
fn ensure_asset(path: &Path, url: &str, label: &str) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }

    if !auto_download_enabled() {
        bail!(
            "{label} not found at {} and automatic download is disabled (set BONES_SEMANTIC_AUTO_DOWNLOAD=1)",
            path.display()
        );
    }

    tracing::info!("downloading {label} to {}", path.display());
    download_to_path(url, path, label)
}

#[cfg(feature = "semantic-model2vec")]
fn auto_download_enabled() -> bool {
    std::env::var("BONES_SEMANTIC_AUTO_DOWNLOAD")
        .ok()
        .is_none_or(|raw| {
            !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
}

#[cfg(feature = "semantic-model2vec")]
fn download_to_path(url: &str, path: &Path, label: &str) -> Result<()> {
    use std::io::Write;
    use std::time::Duration;

    let parent = path.parent().with_context(|| {
        format!(
            "{label} cache path '{}' has no parent directory",
            path.display()
        )
    })?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create {label} cache directory {}",
            parent.display()
        )
    })?;

    let temp_path = parent.join(format!(
        "{}.download",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(60))
        .build();

    let response = match agent
        .get(url)
        .set("User-Agent", "bones-search/model2vec-downloader")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) => {
            bail!("{label} download failed: HTTP {code} from {url}")
        }
        Err(ureq::Error::Transport(err)) => {
            bail!("{label} download failed from {url}: {err}")
        }
    };

    {
        let mut reader = response.into_reader();
        let mut out = std::fs::File::create(&temp_path)
            .with_context(|| format!("failed to create temp file {}", temp_path.display()))?;
        std::io::copy(&mut reader, &mut out)
            .with_context(|| format!("failed to write {label} download"))?;
        out.flush()
            .with_context(|| format!("failed to flush {label} download"))?;
    }

    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to replace existing {label} at {}", path.display()))?;
    }

    std::fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move downloaded {label} from {} to {}",
            temp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "semantic-model2vec")]
    use super::*;

    #[cfg(feature = "semantic-model2vec")]
    #[test]
    fn normalize_l2_produces_unit_norm() {
        let mut v = vec![3.0_f32, 4.0, 0.0];
        normalize_l2(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[cfg(feature = "semantic-model2vec")]
    #[test]
    fn normalize_l2_zero_vector_stays_zero() {
        let mut v = vec![0.0_f32; 10];
        normalize_l2(&mut v);
        assert!(v.iter().all(|&x| x == 0.0));
    }
}
