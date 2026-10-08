//! SQLite plugin configuration structures

use serde::{Deserialize, Serialize};

/// SQLite connection configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SqliteConfig {
    /// Database file path
    pub path: String,
}

impl SqliteConfig {
    pub fn new(path: String) -> Self {
        Self { path }
    }
}
