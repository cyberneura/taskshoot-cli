use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::blocking::{multipart, Client, RequestBuilder};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::config::Config;

const PATH_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn enc(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_ENCODE).to_string()
}

pub struct Api {
    http: Client,
    origin: String,
    org: Option<String>,
    key: String,
}

impl Api {
    pub fn new(config: &Config) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            origin: config.api_origin.clone(),
            org: config.organization.clone(),
            key: config.api_key.clone(),
        })
    }

    pub fn organization(&self) -> Option<&str> {
        self.org.as_deref()
    }

    /// org スコープのエンドポイントで必須。me / orgs は org 無しでも使える。
    fn org(&self) -> Result<&str> {
        self.org
            .as_deref()
            .context("TASKSHOOT_CLI_ORGANIZATION is not set (or pass --org)")
    }

    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.origin, path))
            .bearer_auth(&self.key)
    }

    fn send(&self, builder: RequestBuilder) -> Result<Value> {
        let response = builder.send().context("request failed")?;
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if !status.is_success() {
            bail!("API error {}: {}", status.as_u16(), extract_detail(&text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .with_context(|| format!("invalid JSON response: {}", truncate(&text, 200)))
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.send(self.request(reqwest::Method::GET, path))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.request(reqwest::Method::PATCH, path).json(body))
    }

    fn org_path(&self, rest: &str) -> Result<String> {
        Ok(format!("/api/org/{}/{}", enc(self.org()?), rest))
    }

    fn project_path(&self, project_key: &str, rest: &str) -> Result<String> {
        self.org_path(&format!("projects/{}/{}", enc(project_key), rest))
    }

    fn task_path(&self, project_key: &str, task_ref: &str, rest: &str) -> Result<String> {
        self.project_path(project_key, &format!("tasks/{}{}", enc(task_ref), rest))
    }

    // --- endpoints -----------------------------------------------------

    pub fn me(&self) -> Result<Value> {
        self.get("/api/user/me")
    }

    pub fn orgs(&self) -> Result<Value> {
        self.get("/api/org/")
    }

    pub fn projects(&self) -> Result<Value> {
        self.get(&self.org_path("projects/")?)
    }

    pub fn tasks(
        &self,
        project: &str,
        tracked: Option<bool>,
        limit: u32,
        status: Option<i64>,
        assignee_id: Option<&str>,
    ) -> Result<Value> {
        let mut path = self.project_path(project, &format!("tasks/?limit={limit}"))?;
        if let Some(tracked) = tracked {
            path.push_str(&format!("&tracked={tracked}"));
        }
        if let Some(status) = status {
            path.push_str(&format!("&status={status}"));
        }
        if let Some(assignee_id) = assignee_id {
            path.push_str(&format!("&assignee_id={}", enc(assignee_id)));
        }
        self.get(&path)
    }

    pub fn task_detail(&self, project: &str, task_ref: &str) -> Result<Value> {
        self.get(&self.task_path(project, task_ref, "/detail")?)
    }

    pub fn create_task_full(&self, project: &str, body: &Value) -> Result<Value> {
        self.post(&self.project_path(project, "tasks/full")?, body)
    }

    pub fn create_task_casual(&self, project: &str, content: &str) -> Result<Value> {
        self.post(
            &self.project_path(project, "tasks/")?,
            &json!({ "content": content }),
        )
    }

    pub fn patch_task(&self, project: &str, task_ref: &str, body: &Value) -> Result<Value> {
        self.patch(&self.task_path(project, task_ref, "")?, body)
    }

    pub fn events(&self, project: &str, task_ref: &str) -> Result<Value> {
        self.get(&self.task_path(project, task_ref, "/events/")?)
    }

    pub fn post_comment(&self, project: &str, task_ref: &str, content: &str) -> Result<Value> {
        self.post(
            &self.task_path(project, task_ref, "/events/")?,
            &json!({ "content": content }),
        )
    }

    pub fn post_comment_with_files(
        &self,
        project: &str,
        task_ref: &str,
        content: &str,
        files: &[PathBuf],
    ) -> Result<Value> {
        let mut form = multipart::Form::new().text("content", content.to_string());
        for file in files {
            form = form
                .file("files", file)
                .with_context(|| format!("cannot read file {}", file.display()))?;
        }
        let path = self.task_path(project, task_ref, "/events/upload")?;
        self.send(self.request(reqwest::Method::POST, &path).multipart(form))
    }

    pub fn track(&self, project: &str, task_ref: &str) -> Result<Value> {
        self.post(&self.task_path(project, task_ref, "/track")?, &json!({}))
    }

    pub fn cancel(&self, project: &str, task_ref: &str, reason: &str) -> Result<Value> {
        self.post(
            &self.task_path(project, task_ref, "/cancel")?,
            &json!({ "reason": reason }),
        )
    }

    pub fn resume(&self, project: &str, task_ref: &str) -> Result<Value> {
        self.post(&self.task_path(project, task_ref, "/resume")?, &json!({}))
    }

    pub fn project_workflows(&self, project: &str) -> Result<Value> {
        self.get(&self.project_path(project, "workflows/")?)
    }

    pub fn assignable_users(&self, project: &str) -> Result<Value> {
        self.get(&self.project_path(project, "assignable-users/")?)
    }
}

pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("unexpected API response shape")
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// エラーボディの `{"detail": "..."}` (400/403/404) と
/// pydantic 形式 `{"detail": [...]}` (422) の両方から要点を取り出す。
fn extract_detail(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => match value.get("detail") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => truncate(body.trim(), 300),
        },
        Err(_) => truncate(body.trim(), 300),
    }
}
