use crate::config::ServiceConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    List,
    Get { name: String },
    Add { config: ServiceConfig },
    Start { name: String },
    Stop { name: String },
    Restart { name: String },
    Enable { name: String, enabled: bool },
    Remove { name: String },
    Reload { name: String },
    Shutdown,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Response {
    pub fn success(data: Value) -> Self {
        Self {
            ok: true,
            data,
            error: None,
        }
    }
    pub fn error(error: impl ToString) -> Self {
        Self {
            ok: false,
            data: Value::Null,
            error: Some(error.to_string()),
        }
    }
}
