use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BoxFuture, ChatMessage};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub schema_version: u32,
    pub id: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub preferences: SessionPreferences,
    #[serde(default)]
    pub history: Vec<ChatMessage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub workspace_root: Option<String>,
    pub origin_workspace_root: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub history_len: usize,
    pub has_managed_children: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTarget {
    Last,
    Id(String),
}

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("session `{0}` was not found")]
    NotFound(String),
    #[error("invalid session id `{0}`")]
    InvalidId(String),
    #[error("session is corrupt: {0}")]
    Corrupt(String),
    #[error("session schema is unsupported: {0}")]
    UnsupportedSchema(u32),
    #[error("session store is unavailable: {0}")]
    Unavailable(String),
}

/// Durable session port. Implementations must commit atomically and preserve
/// the previous readable state when a write fails.
pub trait SessionStore: Send + Sync {
    fn load<'a>(
        &'a self,
        target: SessionTarget,
        workspace_root: &'a str,
    ) -> BoxFuture<'a, Result<Session, SessionStoreError>>;

    fn save<'a>(&'a self, session: &'a Session) -> BoxFuture<'a, Result<(), SessionStoreError>>;

    fn list<'a>(
        &'a self,
        workspace_root: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<SessionSummary>, SessionStoreError>>;
}
