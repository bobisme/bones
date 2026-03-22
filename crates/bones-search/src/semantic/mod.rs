//! Semantic search model integration.

mod embed;
mod model;
pub(crate) mod model2vec;
pub mod search;

pub use embed::{
    EmbeddingPipeline, SyncStats, ensure_semantic_index_schema, sync_projection_embeddings,
};
pub use model::{SemanticModel, is_semantic_available};
pub use search::{SemanticSearchResult, knn_search};
