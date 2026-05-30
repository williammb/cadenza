//! Project info exposed over the wire.
//!
//! The app's full `Project` type lives in `src-tauri` (it carries
//! filesystem and agent config the CLI has no business knowing). This is
//! the trimmed, wire-safe view the `list_projects` op returns so an agent
//! can discover the `project_id` to pass to `new-task` / `create-ideia`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub path: String,
}
