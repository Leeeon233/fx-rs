//! Platform-neutral contracts and orchestration for fx.
//!
//! This crate intentionally performs no filesystem, terminal, process, or
//! network I/O. The application runtime and test hosts implement the ports
//! declared here and share the same agent semantics.

mod agent;
mod context;
mod gateway;
mod message;
mod permission;
mod read_evidence;
mod redaction;
mod session;
mod tool;
mod tool_result;

pub use agent::*;
pub use context::*;
pub use gateway::*;
pub use message::*;
pub use permission::*;
pub use read_evidence::*;
pub use redaction::*;
pub use session::*;
pub use tool::*;
pub use tool_result::*;

/// Boxed future used by object-safe asynchronous ports.
///
/// Keeping this alias in core avoids selecting an async executor in the domain
/// crate. The native composition root may use Tokio while embedded hosts may
/// drive the same future with their own executor.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
