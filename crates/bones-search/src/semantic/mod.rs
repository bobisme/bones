//! Semantic search model integration.

mod embed;
mod model;

pub use embed::EmbeddingPipeline;
pub use model::{SemanticModel, is_semantic_available};
