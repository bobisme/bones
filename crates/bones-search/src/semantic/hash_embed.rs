//! Hash-based embedding backend.
//!
//! Generates fixed-dimension vectors using character n-gram feature hashing.
//! No ML model, no external dependencies — always available as a baseline.
//!
//! The approach: split text into overlapping character n-grams (3–5 chars),
//! hash each n-gram to a dimension index, accumulate counts, then L2-normalize.
//! This captures some morphological similarity (words sharing substrings map
//! to nearby vectors) without any trained model.
//!
//! Quality is well below model2vec or ONNX-based embeddings, but it provides
//! *some* semantic signal for search ranking and deduplication when no ML
//! backend is compiled in.

use anyhow::Result;

/// Default embedding dimension for hash embeddings.
const HASH_DIM: usize = 256;

/// Character n-gram range (inclusive).
const NGRAM_MIN: usize = 3;
const NGRAM_MAX: usize = 5;

pub struct HashEmbedBackend {
    dimensions: usize,
}

impl HashEmbedBackend {
    /// Create a new hash embedder with the default dimension.
    pub fn new() -> Self {
        Self {
            dimensions: HASH_DIM,
        }
    }

    /// The dimensionality of embedding vectors this backend produces.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Embed a single text string.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(hash_embed(text, self.dimensions))
    }

    /// Batch-embed multiple texts.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| hash_embed(text, self.dimensions))
            .collect())
    }
}

/// Generate a fixed-dimension embedding from text using character n-gram
/// feature hashing.
fn hash_embed(text: &str, dimensions: usize) -> Vec<f32> {
    let mut vec = vec![0.0_f32; dimensions];
    let normalized = text.to_lowercase();
    let chars: Vec<char> = normalized.chars().collect();

    if chars.is_empty() {
        return vec;
    }

    // Also hash whole whitespace-delimited tokens for word-level signal.
    for word in normalized.split_whitespace() {
        if !word.is_empty() {
            let idx = fnv1a(word.as_bytes()) % dimensions;
            vec[idx] += 1.0;
        }
    }

    // Character n-grams for morphological similarity.
    for n in NGRAM_MIN..=NGRAM_MAX {
        if chars.len() < n {
            continue;
        }
        for window in chars.windows(n) {
            let s: String = window.iter().collect();
            let idx = fnv1a(s.as_bytes()) % dimensions;
            vec[idx] += 1.0;
        }
    }

    normalize_l2(&mut vec);
    vec
}

/// FNV-1a hash — fast, simple, good distribution for feature hashing.
fn fnv1a(bytes: &[u8]) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash as usize
}

fn normalize_l2(values: &mut [f32]) {
    let norm_sq: f32 = values.iter().map(|v| v * v).sum();
    if norm_sq > f32::EPSILON {
        let inv_norm = 1.0 / norm_sq.sqrt();
        for v in values {
            *v *= inv_norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_produces_correct_dimensions() {
        let backend = HashEmbedBackend::new();
        let embedding = backend.embed("hello world").unwrap();
        assert_eq!(embedding.len(), HASH_DIM);
    }

    #[test]
    fn embed_is_normalized() {
        let backend = HashEmbedBackend::new();
        let embedding = backend.embed("some text to embed").unwrap();
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "expected unit norm, got {norm}"
        );
    }

    #[test]
    fn empty_text_produces_zero_vector() {
        let backend = HashEmbedBackend::new();
        let embedding = backend.embed("").unwrap();
        assert!(embedding.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn similar_texts_have_higher_similarity() {
        let backend = HashEmbedBackend::new();
        let a = backend.embed("fix login bug in authentication").unwrap();
        let b = backend.embed("fix auth login issue").unwrap();
        let c = backend.embed("add dark mode to settings page").unwrap();

        let sim_ab = cosine_sim(&a, &b);
        let sim_ac = cosine_sim(&a, &c);

        assert!(
            sim_ab > sim_ac,
            "similar texts should have higher similarity: ab={sim_ab} vs ac={sim_ac}"
        );
    }

    #[test]
    fn batch_matches_individual() {
        let backend = HashEmbedBackend::new();
        let texts = &["hello", "world"];
        let batch = backend.embed_batch(texts).unwrap();
        let individual: Vec<Vec<f32>> = texts
            .iter()
            .map(|t| backend.embed(t).unwrap())
            .collect();
        assert_eq!(batch, individual);
    }

    #[test]
    fn deterministic_output() {
        let backend = HashEmbedBackend::new();
        let a = backend.embed("deterministic test").unwrap();
        let b = backend.embed("deterministic test").unwrap();
        assert_eq!(a, b);
    }

    fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }
}
