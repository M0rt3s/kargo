//! `kargo`: a generic, agent-grade client for voidkargo (<https://voidkargo.com>).
//!
//! The crate holds no knowledge of the game. Commands, fields and rules are read
//! from the server's catalogue at run time and presented three ways: a
//! command-line interface ([`cli`]), a local MCP server on stdio, and a hosted
//! MCP endpoint ([`serve`]).
//!
//! The binary is a thin wrapper around [`cli`]. Only the pieces the binary and
//! the integration tests need are public; everything else is internal.

pub(crate) mod act;
pub(crate) mod api;
pub(crate) mod cache;
pub(crate) mod catalogue;
pub mod cli;
pub(crate) mod config;
pub mod errors;
pub(crate) mod mcp;
pub(crate) mod names;
pub mod output;
pub(crate) mod schema;
pub mod serve;
#[cfg(test)]
mod testutil;
pub(crate) mod token_hash;
pub(crate) mod verifier;
