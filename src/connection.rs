/// SQLite Connection Manager integration.
///
/// Implements ConnectionDataProvider and ConnectionDisplayProvider traits
/// for SQLite plugin to provide connection metadata and display configuration.
use serde_json::Value;
use std::collections::HashMap;

use voidb_core::connection::ConnectionConfig;
use voidb_core::plugin::{
    ConnectionDataProvider, ConnectionDisplayProvider, ConnectionSummary, DisplayLayout,
    FieldDefinition, FieldType, ValidationRule, QuickAction, DisplayConfig,
    ViewMode, DefaultColumn, SortOption, FilterOption,
};

/// SQLite plugin connection data provider
pub struct SqliteConnectionProvider;

impl SqliteConnectionProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SqliteConnectionProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionDataProvider for SqliteConnectionProvider {
    fn plugin_id(&self) -> &str {
        "sqlite"
    }

    fn get_connection_summary(&self, config: &ConnectionConfig) -> ConnectionSummary {
        use crate::config::SqliteConfig;

        let sqlite_config: SqliteConfig = config.plugin_config
            .as_ref()
            .and_then(|pc| serde_json::from_value(pc.clone()).ok())
            .unwrap_or_default();

        let path = &sqlite_config.path;

        // Extract just the filename for display
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);

        ConnectionSummary {
            icon: "📦".to_string(),
            display_name: config.name.clone(),
            type_label: "SQLite".to_string(),
            primary_info: filename.to_string(),
            secondary_info: format!("path: {}", path),
            tags: vec!["database".to_string(), "sql".to_string(), "file".to_string()],
            metadata: HashMap::new(),
            display_layout: DisplayLayout {
                columns: vec![],
                badges: vec![],
                inline_widgets: Vec::new(),
            },
        }
    }

    fn get_custom_fields(&self) -> Vec<FieldDefinition> {
        vec![
            FieldDefinition {
                name: "journal_mode".to_string(),
                label: "Journal Mode".to_string(),
                field_type: FieldType::Select {
                    options: vec![
                        "wal".to_string(),
                        "delete".to_string(),
                        "truncate".to_string(),
                        "persist".to_string(),
                        "memory".to_string(),
                        "off".to_string(),
                    ],
                },
                default_value: Some(serde_json::json!("wal")),
                validation: ValidationRule::OneOf(vec![
                    "wal".to_string(),
                    "delete".to_string(),
                    "truncate".to_string(),
                    "persist".to_string(),
                    "memory".to_string(),
                    "off".to_string(),
                ]),
                help_text: Some("SQLite journal mode for write operations".to_string()),
                required: false,
            },
        ]
    }

    fn validate_custom_field(&self, name: &str, value: &Value) -> Result<(), String> {
        match name {
            "journal_mode" => {
                let mode = value
                    .as_str()
                    .ok_or_else(|| "Journal mode must be a string".to_string())?;
                let valid_modes = ["wal", "delete", "truncate", "persist", "memory", "off"];
                if !valid_modes.contains(&mode) {
                    return Err(format!(
                        "Invalid journal mode '{}'. Must be one of: {}",
                        mode,
                        valid_modes.join(", ")
                    ));
                }
                Ok(())
            }
            _ => Err(format!("Unknown field: {}", name)),
        }
    }

    fn get_quick_actions(&self) -> Vec<QuickAction> {
        vec![
            QuickAction {
                id: "open_sql_editor".to_string(),
                label: "SQL Editor".to_string(),
                icon: Some("📝".to_string()),
                description: Some("Open SQL query editor".to_string()),
            },
            QuickAction {
                id: "show_tables".to_string(),
                label: "Show Tables".to_string(),
                icon: Some("📂".to_string()),
                description: Some("List all tables".to_string()),
            },
            QuickAction {
                id: "connection_info".to_string(),
                label: "Info".to_string(),
                icon: Some("ℹ\u{fe0f}".to_string()),
                description: Some("Show connection details".to_string()),
            },
        ]
    }
}

impl ConnectionDisplayProvider for SqliteConnectionProvider {
    fn plugin_id(&self) -> &str {
        "sqlite"
    }

    fn get_display_config(&self) -> DisplayConfig {
        DisplayConfig {
            view_mode: ViewMode::Table,
            default_columns: vec![
                DefaultColumn::Status,
                DefaultColumn::Icon,
                DefaultColumn::Name,
                DefaultColumn::Type,
                DefaultColumn::Address,
                DefaultColumn::LastUsed,
            ],
            custom_columns: vec![],
            sort_options: vec![
                SortOption::Name,
                SortOption::LastUsed,
                SortOption::Status,
            ],
            filter_options: vec![
                FilterOption::ByStatus,
                FilterOption::ByTag,
            ],
        }
    }

    fn has_custom_rendering(&self) -> bool {
        false
    }
}
