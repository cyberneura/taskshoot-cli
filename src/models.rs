use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub id: String,
    pub email: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Org {
    pub code_name: String,
    pub display_name: String,
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskAuthor {
    pub display_name: String,
    #[serde(default)]
    pub is_bot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_key: String,
    pub number: Option<u64>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub workflow: WorkflowRef,
    pub phase: String,
    pub status: i64,
    pub status_label: String,
    #[serde(default)]
    pub priority_label: String,
    #[serde(default)]
    pub progress: i64,
    pub assignee: Option<TaskAuthor>,
    pub owner: Option<TaskAuthor>,
    pub reporter: Option<TaskAuthor>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    pub tracked: bool,
    #[serde(default)]
    pub latest_event_at: Option<String>,
    #[serde(default)]
    pub event_count: i64,
    pub created_at: String,
}

impl Task {
    /// 表示用 ID。tracked なら `KEY-番号`、untracked は UUID。
    pub fn display_ref(&self) -> String {
        match self.number {
            Some(number) => format!("{}-{}", self.project_key, number),
            None => self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stage {
    pub value: i64,
    pub label: String,
    #[serde(default)]
    pub is_initial: bool,
    #[serde(default)]
    pub is_terminal: bool,
    #[serde(default = "default_true")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workflow {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskEvent {
    pub event_type: String,
    pub author: Option<TaskAuthor>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub filename: String,
    #[serde(default)]
    pub file_size: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssignableUser {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub handle_name: Option<String>,
}
