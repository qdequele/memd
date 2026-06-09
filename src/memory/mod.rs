//! Memory domain: the data model, heuristic classification, and the core
//! memory service (CRUD + hybrid search) shared by the CLI, MCP, and crawler.

pub mod classify;
pub mod model;
pub mod service;

pub use model::{MemoryItem, MemoryType, Source};
pub use service::{GetRequest, MemoryService, ProjectionOptions, QueryResult, SaveRequest};
