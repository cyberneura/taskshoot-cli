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
    #[serde(default)]
    pub category: Option<TaskCategory>,
    pub assignee: Option<TaskAuthor>,
    pub owner: Option<TaskAuthor>,
    pub reporter: Option<TaskAuthor>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub due_date: Option<String>,
    pub tracked: bool,
    /// Bot (AI エージェント) が着手してよいか。
    #[serde(default)]
    pub bot_ready: bool,
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
pub struct SearchResult {
    pub id: String,
    pub project_key: String,
    pub number: Option<u64>,
    pub title: String,
    #[serde(default)]
    pub status_label: String,
}

impl SearchResult {
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
pub struct TaskCategory {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub ordering: i64,
    #[serde(default = "default_true")]
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskEvent {
    pub event_type: String,
    pub author: Option<TaskAuthor>,
    #[serde(default)]
    pub content: String,
    /// field_change 等は content が空で、変更内容は metadata.changes に入る
    #[serde(default)]
    pub metadata: serde_json::Value,
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

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationTask {
    pub project_key: String,
    #[serde(default)]
    pub number: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Notification {
    pub id: String,
    pub notification_type: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub read: bool,
    pub created_at: String,
    #[serde(default)]
    pub task: Option<NotificationTask>,
}

impl Notification {
    /// 関連タスクの表示 ref (`KEY-N`。untracked / タスク無しは "-")。
    pub fn task_ref(&self) -> String {
        match &self.task {
            Some(task) => match task.number {
                Some(number) => format!("{}-{}", task.project_key, number),
                None => task.project_key.clone(),
            },
            None => "-".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationList {
    pub items: Vec<Notification>,
    pub unread_count: i64,
}
