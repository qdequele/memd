//! Structured knowledge: entities (typed records in the `memories` index) and
//! the typed relations between them (the `memory_relations` index).
//!
//! memd stays model-free: agents supply names, kinds and relations; this
//! module normalises identities, resolves names, and answers `explore`.

pub mod ident;
pub mod relations;
pub mod resolve;
pub mod service;
#[allow(unused_imports)] // first user: MCP layer, Task 8; removed there
pub use service::KnowledgeService;
