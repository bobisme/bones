use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "semantic-ort")]
use ort::{session::Session, value::Tensor};
#[cfg(feature = "semantic-ort")]
use std::io::Write;
#[cfg(feature = "semantic-ort")]
use std::sync::Mutex;
#[cfg(feature = "semantic-ort")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "semantic-ort")]
use std::time::Duration;
#[cfg(feature = "semantic-ort")]
use tokenizers::Tokenizer;

const MODEL_FILENAME: &str = "minilm-l6-v2-int8.onnx";
#[cfg(feature = "semantic-ort")]
const TOKENIZER_FILENAME: &str = "minilm-l6-v2-tokenizer.json";
#[cfg(feature = "semantic-ort")]
const MAX_TOKENS: usize = 256;
#[cfg(feature = "semantic-ort")]
const MODEL_DOWNLOAD_URL_ENV: &str = "BONES_SEMANTIC_MODEL_URL";
#[cfg(feature = "semantic-ort")]
const TOKENIZER_DOWNLOAD_URL_ENV: &str = "BONES_SEMANTIC_TOKENIZER_URL";
#[cfg(feature = "semantic-ort")]
const AUTO_DOWNLOAD_ENV: &str = "BONES_SEMANTIC_AUTO_DOWNLOAD";
#[cfg(feature = "semantic-ort")]
const MODEL_DOWNLOAD_URL_DEFAULT: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx";
#[cfg(feature = "semantic-ort")]
const TOKENIZER_DOWNLOAD_URL_DEFAULT: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
#[cfg(feature = "semantic-ort")]
const DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 2;
#[cfg(feature = "semantic-ort")]
const DOWNLOAD_READ_TIMEOUT_SECS: u64 = 30;

#[cfg(feature = "semantic-ort")]
static MODEL_DOWNLOAD_ATTEMPTED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "semantic-ort")]
static TOKENIZER_DOWNLOAD_ATTEMPTED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "bundled-model")]
const BUNDLED_MODEL_BYTES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/minilm-l6-v2-int8.onnx"
));

/// Wrapper around an ONNX Runtime session for the embedding model.
pub struct SemanticModel {
    #[cfg(feature = "semantic-ort")]
    session: Mutex<Session>,
    #[cfg(feature = "semantic-ort")]
    tokenizer: Tokenizer,
}

#[cfg(feature = "semantic-ort")]
struct EncodedText {
    input_ids: Vec<i64>,
    attention_mask: Vec<i64>,
}

#[cfg(feature = "semantic-ort")]
enum InputSource {
    InputIds,
    AttentionMask,
    TokenTypeIds,
}

impl SemanticModel {
    /// Load the model from the OS cache directory.
    ///
    /// If the cache file is missing or has a mismatched SHA256, this extracts
    /// bundled model bytes into the cache directory before creating an ORT
    /// session. When no bundled bytes are available, this attempts a one-time
    /// download of model/tokenizer assets from stable URLs.
    pub fn load() -> Result<Self> {
        let path = Self::model_cache_path()?;
        Self::ensure_model_cached(&path)?;

        #[cfg(feature = "semantic-ort")]
        {
            let tokenizer_path = Self::tokenizer_cache_path()?;
            Self::ensure_tokenizer_cached(&tokenizer_path)?;

            let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
                anyhow!(
                    "failed to load semantic tokenizer from {}: {e}",
                    tokenizer_path.display()
                )
            })?;

            let session = Session::builder()
                .context("failed to create ONNX Runtime session builder")?
                .commit_from_file(&path)
                .with_context(|| {
                    format!("failed to load semantic model from {}", path.display())
                })?;

            return Ok(Self {
                session: Mutex::new(session),
                tokenizer,
            });
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
        Ok(Self::model_cache_root()?.join(MODEL_FILENAME))
    }

    #[cfg(feature = "semantic-ort")]
    fn tokenizer_cache_path() -> Result<PathBuf> {
        Ok(Self::model_cache_root()?.join(TOKENIZER_FILENAME))
    }

    fn model_cache_root() -> Result<PathBuf> {
        let mut path = dirs::cache_dir().context("unable to determine OS cache directory")?;
        path.push("bones");
        path.push("models");
        Ok(path)
    }

    /// Check if cached model matches expected SHA256.
    pub fn is_cached_valid(path: &Path) -> bool {
        let expected_sha256 = expected_model_sha256();
        if expected_sha256.is_none() {
            return path.is_file();
        }

        let Ok(contents) = fs::read(path) else {
            return false;
        };

        expected_sha256.is_some_and(|sha256| sha256_hex(&contents) == sha256)
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

    fn ensure_model_cached(path: &Path) -> Result<()> {
        if Self::is_cached_valid(path) {
            return Ok(());
        }

        if bundled_model_bytes().is_some() {
            Self::extract_to_cache(path)?;
            return Ok(());
        }

        #[cfg(feature = "semantic-ort")]
        {
            if !auto_download_enabled() {
                bail!(
                    "semantic model not found at {}. Automatic download is disabled via {AUTO_DOWNLOAD_ENV}=0",
                    path.display()
                );
            }

            if MODEL_DOWNLOAD_ATTEMPTED.swap(true, Ordering::SeqCst) {
                bail!(
                    "semantic model not found at {} and auto-download was already attempted in this process",
                    path.display()
                );
            }

            download_to_path(&model_download_url(), path, "semantic model")
                .with_context(|| format!("failed to fetch semantic model to {}", path.display()))?;

            if !Self::is_cached_valid(path) {
                bail!(
                    "downloaded semantic model at {} failed validation",
                    path.display()
                );
            }

            return Ok(());
        }

        #[cfg(not(feature = "semantic-ort"))]
        {
            bail!(
                "semantic model not found at {}; enable `bundled-model` or place `{MODEL_FILENAME}` in the cache path",
                path.display()
            );
        }
    }

    #[cfg(feature = "semantic-ort")]
    fn ensure_tokenizer_cached(path: &Path) -> Result<()> {
        if path.is_file() {
            return Ok(());
        }

        if !auto_download_enabled() {
            bail!(
                "semantic tokenizer not found at {}. Automatic download is disabled via {AUTO_DOWNLOAD_ENV}=0",
                path.display()
            );
        }

        if TOKENIZER_DOWNLOAD_ATTEMPTED.swap(true, Ordering::SeqCst) {
            bail!(
                "semantic tokenizer not found at {} and auto-download was already attempted in this process",
                path.display()
            );
        }

        download_to_path(&tokenizer_download_url(), path, "semantic tokenizer")
            .with_context(|| format!("failed to fetch semantic tokenizer to {}", path.display()))?;

        if !path.is_file() {
            bail!("semantic tokenizer download completed but file was not created");
        }

        Ok(())
    }

    /// Run inference for a single text input.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        #[cfg(feature = "semantic-ort")]
        {
            let encoded = self.encode_text(text)?;
            let mut out = self.run_model_batch(&[encoded])?;
            return out
                .pop()
                .ok_or_else(|| anyhow!("semantic model returned no embedding"));
        }

        #[cfg(not(feature = "semantic-ort"))]
        {
            let _ = self;
            let _ = text;
            bail!("semantic runtime unavailable: compile bones-search with `semantic-ort`");
        }
    }

    /// Batch inference for efficiency.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        #[cfg(feature = "semantic-ort")]
        {
            let encoded: Vec<EncodedText> = texts
                .iter()
                .map(|text| self.encode_text(text))
                .collect::<Result<Vec<_>>>()?;
            return self.run_model_batch(&encoded);
        }

        #[cfg(not(feature = "semantic-ort"))]
        {
            let _ = self;
            let _ = texts;
            bail!("semantic runtime unavailable: compile bones-search with `semantic-ort`");
        }
    }

    #[cfg(feature = "semantic-ort")]
    fn encode_text(&self, text: &str) -> Result<EncodedText> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("failed to tokenize semantic query: {e}"))?;

        let ids = encoding.get_ids();
        if ids.is_empty() {
            bail!("semantic tokenizer produced zero tokens");
        }

        let attention = encoding.get_attention_mask();
        let keep = ids.len().min(MAX_TOKENS);

        let mut input_ids = Vec::with_capacity(keep);
        let mut attention_mask = Vec::with_capacity(keep);
        for idx in 0..keep {
            input_ids.push(i64::from(ids[idx]));
            attention_mask.push(i64::from(*attention.get(idx).unwrap_or(&1_u32)));
        }
        if attention_mask.iter().all(|v| *v == 0) {
            attention_mask.fill(1);
        }

        Ok(EncodedText {
            input_ids,
            attention_mask,
        })
    }

    #[cfg(feature = "semantic-ort")]
    fn run_model_batch(&self, encoded: &[EncodedText]) -> Result<Vec<Vec<f32>>> {
        if encoded.is_empty() {
            return Ok(Vec::new());
        }

        let batch = encoded.len();
        let seq_len = encoded.iter().map(|e| e.input_ids.len()).max().unwrap_or(0);
        if seq_len == 0 {
            bail!("semantic batch has no tokens");
        }

        let mut flat_ids = vec![0_i64; batch * seq_len];
        let mut flat_attention = vec![0_i64; batch * seq_len];
        for (row_idx, row) in encoded.iter().enumerate() {
            let row_base = row_idx * seq_len;
            for col_idx in 0..row.input_ids.len() {
                flat_ids[row_base + col_idx] = row.input_ids[col_idx];
                flat_attention[row_base + col_idx] = row.attention_mask[col_idx];
            }
        }
        let flat_token_types = vec![0_i64; batch * seq_len];

        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("semantic model session mutex poisoned"))?;

        let model_inputs = session.inputs();
        let mut inputs: Vec<(String, Tensor<i64>)> = Vec::with_capacity(model_inputs.len());
        for (index, input) in model_inputs.iter().enumerate() {
            let input_name = input.name();
            let source = input_source(index, input_name);
            let data = match source {
                InputSource::InputIds => flat_ids.clone(),
                InputSource::AttentionMask => flat_attention.clone(),
                InputSource::TokenTypeIds => flat_token_types.clone(),
            };
            let tensor = Tensor::<i64>::from_array(([batch, seq_len], data.into_boxed_slice()))
                .with_context(|| format!("failed to build ONNX input tensor '{input_name}'"))?;
            inputs.push((input_name.to_string(), tensor));
        }

        let outputs = session
            .run(inputs)
            .context("failed to run ONNX semantic inference")?;

        if outputs.len() == 0 {
            bail!("semantic model returned no outputs");
        }

        let output = outputs
            .get("sentence_embedding")
            .or_else(|| outputs.get("last_hidden_state"))
            .or_else(|| outputs.get("token_embeddings"))
            .unwrap_or(&outputs[0]);

        let (shape, data) = output.try_extract_tensor::<f32>().with_context(
            || "semantic model output tensor is not f32; expected sentence embedding tensor",
        )?;

        decode_embeddings(shape, data, &flat_attention, batch, seq_len)
    }
}

#[cfg(feature = "semantic-ort")]
fn input_source(index: usize, input_name: &str) -> InputSource {
    let name = input_name.to_ascii_lowercase();
    if name.contains("attention") {
        return InputSource::AttentionMask;
    }
    if name.contains("token_type") || name.contains("segment") {
        return InputSource::TokenTypeIds;
    }
    if name.contains("input_ids") || (name.contains("input") && name.contains("id")) {
        return InputSource::InputIds;
    }

    match index {
        0 => InputSource::InputIds,
        1 => InputSource::AttentionMask,
        _ => InputSource::TokenTypeIds,
    }
}

#[cfg(feature = "semantic-ort")]
fn decode_embeddings(
    shape: &[i64],
    data: &[f32],
    flat_attention: &[i64],
    batch: usize,
    seq_len: usize,
) -> Result<Vec<Vec<f32>>> {
    match shape.len() {
        // [batch, hidden]
        2 => {
            let out_batch = usize::try_from(shape[0]).unwrap_or(0);
            let hidden = usize::try_from(shape[1]).unwrap_or(0);
            if out_batch == 0 || hidden == 0 {
                bail!("invalid sentence embedding output shape {shape:?}");
            }
            if out_batch != batch {
                bail!("semantic output batch mismatch: expected {batch}, got {out_batch}");
            }

            let mut out = Vec::with_capacity(out_batch);
            for row in 0..out_batch {
                let start = row * hidden;
                let end = start + hidden;
                let mut emb = data[start..end].to_vec();
                normalize_l2(&mut emb);
                out.push(emb);
            }
            Ok(out)
        }

        // [batch, tokens, hidden] -> mean pool with attention mask.
        3 => {
            let out_batch = usize::try_from(shape[0]).unwrap_or(0);
            let out_tokens = usize::try_from(shape[1]).unwrap_or(0);
            let hidden = usize::try_from(shape[2]).unwrap_or(0);
            if out_batch == 0 || out_tokens == 0 || hidden == 0 {
                bail!("invalid token embedding output shape {shape:?}");
            }
            if out_batch != batch {
                bail!("semantic output batch mismatch: expected {batch}, got {out_batch}");
            }

            let mut out = Vec::with_capacity(out_batch);
            for b in 0..out_batch {
                let mut emb = vec![0.0_f32; hidden];
                let mut weight_sum = 0.0_f32;

                for t in 0..out_tokens {
                    let mask_weight = if t < seq_len {
                        flat_attention[b * seq_len + t] as f32
                    } else {
                        0.0
                    };
                    if mask_weight <= 0.0 {
                        continue;
                    }

                    let token_base = (b * out_tokens + t) * hidden;
                    for h in 0..hidden {
                        emb[h] += data[token_base + h] * mask_weight;
                    }
                    weight_sum += mask_weight;
                }

                if weight_sum > 0.0 {
                    for value in &mut emb {
                        *value /= weight_sum;
                    }
                }
                normalize_l2(&mut emb);
                out.push(emb);
            }
            Ok(out)
        }

        // [hidden] (single-row fallback)
        1 => {
            if batch != 1 {
                bail!("rank-1 semantic output only supported for single-row batch");
            }
            let hidden = usize::try_from(shape[0]).unwrap_or(0);
            if hidden == 0 {
                bail!("invalid rank-1 semantic output shape {shape:?}");
            }
            let mut emb = data[0..hidden].to_vec();
            normalize_l2(&mut emb);
            Ok(vec![emb])
        }

        rank => bail!("unsupported semantic output rank {rank}: shape {shape:?}"),
    }
}

#[cfg(feature = "semantic-ort")]
fn normalize_l2(values: &mut [f32]) {
    let mut norm_sq = 0.0_f32;
    for value in values.iter() {
        norm_sq += value * value;
    }

    if norm_sq == 0.0 {
        return;
    }

    let norm = norm_sq.sqrt();
    for value in values {
        *value /= norm;
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

#[cfg(feature = "semantic-ort")]
fn auto_download_enabled() -> bool {
    std::env::var(AUTO_DOWNLOAD_ENV).ok().is_none_or(|raw| {
        !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

#[cfg(feature = "semantic-ort")]
fn model_download_url() -> String {
    std::env::var(MODEL_DOWNLOAD_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| MODEL_DOWNLOAD_URL_DEFAULT.to_string())
}

#[cfg(feature = "semantic-ort")]
fn tokenizer_download_url() -> String {
    std::env::var(TOKENIZER_DOWNLOAD_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| TOKENIZER_DOWNLOAD_URL_DEFAULT.to_string())
}

#[cfg(feature = "semantic-ort")]
fn download_to_path(url: &str, path: &Path, artifact_label: &str) -> Result<()> {
    let parent = path.parent().with_context(|| {
        format!(
            "{artifact_label} cache path '{}' has no parent directory",
            path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create {} cache directory {}",
            artifact_label,
            parent.display()
        )
    })?;

    let temp_path = parent.join(format!(
        "{}.download",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(DOWNLOAD_CONNECT_TIMEOUT_SECS))
        .timeout_read(Duration::from_secs(DOWNLOAD_READ_TIMEOUT_SECS))
        .build();

    let response = match agent
        .get(url)
        .set("User-Agent", "bones-search/semantic-downloader")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) => {
            bail!("{artifact_label} download failed: HTTP {code} from {url}")
        }
        Err(ureq::Error::Transport(err)) => {
            bail!("{artifact_label} download failed from {url}: {err}")
        }
    };

    {
        let mut reader = response.into_reader();
        let mut out = fs::File::create(&temp_path)
            .with_context(|| format!("failed to create temporary file {}", temp_path.display()))?;
        std::io::copy(&mut reader, &mut out)
            .with_context(|| format!("failed to write {} download", artifact_label))?;
        out.flush()
            .with_context(|| format!("failed to flush {} download", artifact_label))?;
    }

    if path.exists() {
        fs::remove_file(path).with_context(|| {
            format!(
                "failed to replace existing {} at {}",
                artifact_label,
                path.display()
            )
        })?;
    }

    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move downloaded {} from {} to {}",
            artifact_label,
            temp_path.display(),
            path.display()
        )
    })?;

    Ok(())
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
    fn cached_model_is_accepted_when_not_bundled() {
        let tmp = tempfile::tempdir().expect("tempdir must be created");
        let model = tmp.path().join("minilm-l6-v2-int8.onnx");
        fs::write(&model, b"anything").expect("test file should be writable");

        assert!(SemanticModel::is_cached_valid(&model));
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

    #[cfg(feature = "semantic-ort")]
    #[test]
    fn normalize_l2_produces_unit_norm() {
        let mut emb = vec![3.0_f32, 4.0_f32, 0.0_f32];
        normalize_l2(&mut emb);
        let norm = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[cfg(feature = "semantic-ort")]
    #[test]
    fn input_source_prefers_named_fields() {
        assert!(matches!(
            input_source(5, "attention_mask"),
            InputSource::AttentionMask
        ));
        assert!(matches!(
            input_source(5, "token_type_ids"),
            InputSource::TokenTypeIds
        ));
        assert!(matches!(
            input_source(5, "input_ids"),
            InputSource::InputIds
        ));
    }
}
