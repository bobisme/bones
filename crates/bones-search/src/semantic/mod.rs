//! Semantic search model integration.

mod embed;
mod model;
pub mod search;

pub use embed::EmbeddingPipeline;
pub use model::{SemanticModel, is_semantic_available};
pub use search::{SemanticSearchResult, knn_search};
