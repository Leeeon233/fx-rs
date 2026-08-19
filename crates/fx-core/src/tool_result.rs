use thiserror::Error;

pub const LARGE_TOOL_RESULT_BYTES: usize = 16 * 1024;
pub const TOOL_RESULT_PREVIEW_BYTES: usize = 4 * 1024;
pub const TOOL_RESULT_DEFAULT_READ_BYTES: usize = 8 * 1024;
pub const TOOL_RESULT_MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredToolResult {
    pub handle: String,
    pub stored_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultPage {
    pub content: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub total_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultMatch {
    pub line: usize,
    pub content: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ToolResultStoreError {
    #[error("invalid tool-result handle")]
    InvalidHandle,
    #[error("tool-result query must not be empty")]
    InvalidQuery,
    #[error("tool-result handle was not found")]
    NotFound,
    #[error("tool result exceeds the storage limit")]
    TooLarge,
    #[error("tool-result store is unavailable: {0}")]
    Unavailable(String),
}

/// Session-scoped durable storage for complete textual tool results.
///
/// The port is synchronous because results are bounded and local adapters use
/// atomic filesystem operations. Network-backed implementations should stage
/// asynchronously before returning a [`crate::ToolOutput`] to the Agent.
pub trait ToolResultStore: Send + Sync {
    fn store(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        content: &str,
    ) -> Result<StoredToolResult, ToolResultStoreError>;

    fn read_range(
        &self,
        handle: &str,
        start_byte: usize,
        byte_count: usize,
    ) -> Result<ToolResultPage, ToolResultStoreError>;

    fn search(
        &self,
        handle: &str,
        query: &str,
        max_matches: usize,
    ) -> Result<Vec<ToolResultMatch>, ToolResultStoreError>;
}
