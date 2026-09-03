//! MCP server for GFS.
//!
//! Exposes tools: list_providers, status, commit, log, checkout, init, compute.
//! Each tool delegates to domain use cases; the MCP layer is a thin adapter.
//!
//! The handler follows the same pattern as other rmcp HTTP services: a struct with
//! `#[tool_router]` + tools and `impl ServerHandler` with `get_info()`, then served
//! over stdio or streamable HTTP (see [docs/http-service.md](docs/http-service.md)).

mod tools;

pub use tools::GfsMcpHandler;
