use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::api::{from_value, Api};
use crate::models::{AssignableUser, Me, Org, Project, Task, TaskEvent, Workflow};
use crate::output::{print_table, truncate_width};
use crate::stages;
use crate::taskref::{parse_task_ref, TaskRef};

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// タスク引数 (KEY-N or UUID) からプロジェクトキーと API 用 task_ref を得る。
/// スラッグはキーを内包するので --project より優先。UUID は --project が必須。
fn resolve_target(task_arg: &str, project: Option<&str>) -> Result<(String, String)> {
    let task_ref = parse_task_ref(task_arg)?;
    match &task_ref {
        TaskRef::Slug { project_key, .. } => {
            if let Some(project) = project {
                if project != project_key {
                    bail!("--project {project} conflicts with the project key in '{task_arg}'");
                }
            }
            Ok((project_key.clone(), task_ref.api_ref()))
        }
        TaskRef::Uuid(_) => {
            let project =
                project.context("--project is required when referencing a task by UUID")?;
            Ok((project.to_string(), task_ref.api_ref()))
        }
    }
}

/// assignee/owner 指定の解決: "me" → 自分、UUID → そのまま、
/// それ以外は assignable-users の handle_name / display_name 一致 (大文字小文字無視)。
fn resolve_user_id(api: &Api, project: &str, spec: &str) -> Result<String> {
    if spec == "me" {
        let me: Me = from_value(api.me()?)?;
        return Ok(me.id);
    }
    if Uuid::parse_str(spec).is_ok() {
        return Ok(spec.to_string());
    }
    let users: Vec<AssignableUser> = from_value(api.assignable_users(project)?)?;
    let needle = spec.to_lowercase();
    let matches: Vec<&AssignableUser> = users
        .iter()
        .filter(|u| {
            u.display_name.to_lowercase() == needle
                || u.handle_name
                    .as_deref()
                    .is_some_and(|h| h.to_lowercase() == needle)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => bail!("no user matched '{spec}' in project {project}"),
        _ => bail!(
            "ambiguous user '{spec}': {}",
            matches
                .iter()
                .map(|u| u.display_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn task_workflow_stages(
    api: &Api,
    project: &str,
    task_workflow_id: &Option<String>,
) -> Result<Vec<crate::models::Stage>> {
    let workflows: Vec<Workflow> = from_value(api.project_workflows(project)?)?;
    Ok(stages::stages_for_workflow(&workflows, task_workflow_id)?.to_vec())
}

fn format_change_value(value: &Value) -> String {
    match value {
        Value::Null => "-".to_string(),
        Value::String(s) if s.is_empty() => "-".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn author_name(author: &Option<crate::models::TaskAuthor>) -> String {
    author
        .as_ref()
        .map(|a| {
            if a.is_bot {
                format!("{} [bot]", a.display_name)
            } else {
                a.display_name.clone()
            }
        })
        .unwrap_or_else(|| "-".to_string())
}

fn print_task_line(value: &Value, verb: &str) -> Result<()> {
    let task: Task = from_value(value.clone())?;
    println!(
        "{} {}: {} / status: {} ({}) / phase: {} / assignee: {}",
        verb,
        task.display_ref(),
        task.title,
        task.status_label,
        task.status,
        task.phase,
        author_name(&task.assignee),
    );
    Ok(())
}

// --- commands ----------------------------------------------------------

pub fn me(api: &Api, json: bool) -> Result<()> {
    let value = api.me()?;
    if json {
        return print_json(&value);
    }
    let me: Me = from_value(value)?;
    println!("{} <{}>", me.display_name, me.email);
    println!("id:           {}", me.id);
    println!("organization: {}", api.organization().unwrap_or("-"));
    Ok(())
}

pub fn orgs(api: &Api, json: bool) -> Result<()> {
    let value = api.orgs()?;
    if json {
        return print_json(&value);
    }
    let items: Vec<Org> = from_value(value)?;
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|o| {
            vec![
                o.code_name.clone(),
                o.display_name.clone(),
                o.role.clone().unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["CODE", "NAME", "ROLE"], &rows);
    Ok(())
}

pub fn projects(api: &Api, json: bool) -> Result<()> {
    let value = api.projects()?;
    if json {
        return print_json(&value);
    }
    let items: Vec<Project> = from_value(value)?;
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|p| {
            vec![
                p.key.clone(),
                p.name.clone(),
                if p.is_default {
                    "default".to_string()
                } else {
                    String::new()
                },
                if p.active {
                    "active".to_string()
                } else {
                    "inactive".to_string()
                },
            ]
        })
        .collect();
    print_table(&["KEY", "NAME", "DEFAULT", "STATE"], &rows);
    Ok(())
}

pub fn workflows(api: &Api, project: &str, json: bool) -> Result<()> {
    let value = api.project_workflows(project)?;
    if json {
        return print_json(&value);
    }
    let flows: Vec<Workflow> = from_value(value)?;
    for flow in &flows {
        let mut marks: Vec<&str> = Vec::new();
        if flow.is_default {
            marks.push("project default");
        }
        if !flow.active {
            marks.push("inactive");
        }
        let suffix = if marks.is_empty() {
            String::new()
        } else {
            format!(" ({})", marks.join(", "))
        };
        println!("{}{}", flow.name, suffix);
        for stage in &flow.stages {
            if !stage.active {
                continue;
            }
            let mut flags: Vec<&str> = Vec::new();
            if stage.is_initial {
                flags.push("initial");
            }
            if stage.is_terminal {
                flags.push("terminal");
            }
            let flag_str = if flags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", flags.join(","))
            };
            println!("  {:>5}  {}{}", stage.value, stage.label, flag_str);
        }
        println!();
    }
    Ok(())
}

pub struct TasksFilter {
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub untracked: bool,
    pub tracked: bool,
    pub bot_ready: Option<bool>,
    pub limit: Option<u32>,
}

/// 一覧フィルタ用の status 解決。ラベルはプロジェクトの全 workflow から引き、
/// 複数フローで同一ラベルが異なる値になる場合はエラー (数値指定を促す)。
fn resolve_list_status(api: &Api, project: &str, input: &str) -> Result<i64> {
    let input = input.trim();
    if let Ok(value) = input.parse::<i64>() {
        return Ok(value);
    }
    let workflows: Vec<Workflow> = from_value(api.project_workflows(project)?)?;
    let mut matches: Vec<(i64, String)> = Vec::new();
    for flow in &workflows {
        if let Some(stage) = flow.stages.iter().find(|s| s.active && s.label == input) {
            if !matches.iter().any(|(value, _)| *value == stage.value) {
                matches.push((stage.value, flow.name.clone()));
            }
        }
    }
    match matches.len() {
        1 => Ok(matches[0].0),
        0 => bail!("unknown status '{input}' in this project's workflows"),
        _ => bail!(
            "status label '{input}' maps to different values across workflows ({}); \
             specify a numeric value",
            matches
                .iter()
                .map(|(value, flow)| format!("{value} in {flow}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn tasks(api: &Api, project: &str, filter: &TasksFilter, json: bool) -> Result<()> {
    let tracked = if filter.untracked {
        Some(false)
    } else if filter.tracked {
        Some(true)
    } else {
        None
    };
    // status / assignee はサーバー側フィルタに解決して渡す
    let status = match &filter.status {
        Some(status) => Some(resolve_list_status(api, project, status)?),
        None => None,
    };
    let assignee_id = match &filter.assignee {
        Some(assignee) => Some(resolve_user_id(api, project, assignee)?),
        None => None,
    };
    let has_filter = status.is_some() || assignee_id.is_some() || filter.bot_ready.is_some();
    let limit = filter
        .limit
        .unwrap_or(if has_filter { 500 } else { 200 })
        .clamp(1, 500);
    let value = api.tasks(
        project,
        tracked,
        limit,
        status,
        assignee_id.as_deref(),
        filter.bot_ready,
    )?;
    // --json でも API の生フィールドを保つため Value のまま扱う
    let mut items: Vec<Value> = from_value(value)?;
    // limit 件ちょうど返ってきた場合、それより古い一致タスクが切れている可能性がある
    if items.len() as u32 >= limit {
        eprintln!(
            "warning: result may be truncated to the newest {limit} tasks; \
             raise --limit (max 500) if you need more"
        );
    }
    // ラベル指定時はラベル完全一致で再フィルタする。サーバーは数値でしか絞れず、
    // 別 workflow が同じ数値に別ラベルを使っているとその分が混入するため
    if let Some(label) = filter
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| s.parse::<i64>().is_err())
    {
        items.retain(|t| t["status_label"].as_str() == Some(label));
    }

    if json {
        return print_json(&Value::Array(items));
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            let task: Task = from_value(item.clone())?;
            let updated = task
                .latest_event_at
                .as_deref()
                .unwrap_or(&task.created_at)
                .chars()
                .take(10)
                .collect::<String>();
            Ok(vec![
                task.display_ref(),
                task.status_label.clone(),
                format!("{}%", task.progress),
                author_name(&task.assignee),
                truncate_width(&task.title, 48),
                updated,
            ])
        })
        .collect::<Result<_>>()?;
    print_table(
        &["REF", "STATUS", "PROG", "ASSIGNEE", "TITLE", "UPDATED"],
        &rows,
    );
    Ok(())
}

pub fn show(api: &Api, task_arg: &str, project: Option<&str>, json: bool) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let value = api.task_detail(&project, &task_ref)?;
    if json {
        return print_json(&value);
    }
    let task: Task = from_value(value.clone())?;
    println!("{}  {}", task.display_ref(), task.title);
    println!(
        "project:  {} / workflow: {}",
        task.project_key, task.workflow.name
    );
    println!(
        "phase:    {} / status: {} ({}) / progress: {}%",
        task.phase, task.status_label, task.status, task.progress
    );
    println!("priority: {}", task.priority_label);
    println!("assignee: {}", author_name(&task.assignee));
    println!("owner:    {}", author_name(&task.owner));
    println!("reporter: {}", author_name(&task.reporter));
    if !task.labels.is_empty() {
        println!("labels:   {}", task.labels.join(", "));
    }
    if let Some(due) = &task.due_date {
        println!("due:      {}", due);
    }
    println!(
        "tracked:  {} / bot_ready: {} / events: {} / id: {}",
        task.tracked, task.bot_ready, task.event_count, task.id
    );
    if !task.description.is_empty() {
        println!("\n{}", task.description);
    }
    if let Some(fields) = value.get("custom_fields").and_then(|v| v.as_array()) {
        let filled: Vec<&Value> = fields.iter().filter(|f| !f["value"].is_null()).collect();
        if !filled.is_empty() {
            println!("\ncustom fields:");
            for field in filled {
                println!(
                    "  {}: {}",
                    field["name"].as_str().unwrap_or("?"),
                    field["value"]
                );
            }
        }
    }
    Ok(())
}

pub struct CreateArgs {
    pub project: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub owner: Option<String>,
    pub priority: Option<i64>,
    pub due_date: Option<String>,
    pub labels: Vec<String>,
}

pub fn create(api: &Api, args: &CreateArgs, json: bool) -> Result<()> {
    if let Some(content) = &args.content {
        if args.title.is_some() {
            bail!("--content (untracked casual task) and --title (tracked task) are mutually exclusive");
        }
        let value = api.create_task_casual(&args.project, content)?;
        if json {
            return print_json(&value);
        }
        return print_task_line(&value, "created (untracked)");
    }
    let title = args
        .title
        .as_ref()
        .context("--title is required (or use --content for an untracked casual task)")?;

    let mut body = Map::new();
    body.insert("title".to_string(), json!(title));
    if let Some(description) = &args.description {
        body.insert("description".to_string(), json!(description));
    }
    if let Some(assignee) = &args.assignee {
        body.insert(
            "assignee_id".to_string(),
            json!(resolve_user_id(api, &args.project, assignee)?),
        );
    }
    if let Some(owner) = &args.owner {
        body.insert(
            "owner_id".to_string(),
            json!(resolve_user_id(api, &args.project, owner)?),
        );
    }
    if let Some(priority) = args.priority {
        body.insert("priority".to_string(), json!(priority));
    }
    if let Some(due_date) = &args.due_date {
        body.insert("due_date".to_string(), json!(due_date));
    }
    if !args.labels.is_empty() {
        body.insert("labels".to_string(), json!(args.labels));
    }
    let mut value = api.create_task_full(&args.project, &Value::Object(body))?;
    // 作成 API は init_lifecycle が status を初期ステージへリセットするため、
    // --status 指定は作成後の PATCH で反映する。
    // PATCH が失敗しても作成自体は成功しているので、エラーで落とさず警告に留める
    // (エラー終了するとリトライで重複タスクが作られる)。
    if let Some(status) = &args.status {
        let task: Task = from_value(value.clone())?;
        let follow_up = task_workflow_stages(api, &args.project, &task.workflow.id)
            .and_then(|stages| stages::resolve_status(status, &stages))
            .and_then(|status_value| {
                api.patch_task(&args.project, &task.id, &json!({ "status": status_value }))
            });
        match follow_up {
            Ok(patched) => value = patched,
            Err(error) => eprintln!(
                "warning: task {} was created but --status could not be applied: {error:#}",
                task.display_ref()
            ),
        }
    }
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "created")
}

pub struct UpdateArgs {
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub owner: Option<String>,
    pub progress: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub due_date: Option<String>,
    pub labels: Option<Vec<String>>,
    pub bot_ready: Option<bool>,
}

pub fn update(
    api: &Api,
    task_arg: &str,
    project: Option<&str>,
    args: &UpdateArgs,
    json: bool,
) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let mut body = Map::new();
    if let Some(status) = &args.status {
        let status_value = if let Ok(v) = status.trim().parse::<i64>() {
            v
        } else {
            // ラベル解決はタスクの workflow のステージ集合で行う
            let task: Task = from_value(api.task_detail(&project, &task_ref)?)?;
            let stages = task_workflow_stages(api, &project, &task.workflow.id)?;
            stages::resolve_status(status, &stages)?
        };
        body.insert("status".to_string(), json!(status_value));
    }
    if let Some(assignee) = &args.assignee {
        body.insert(
            "assignee_id".to_string(),
            json!(resolve_user_id(api, &project, assignee)?),
        );
    }
    if let Some(owner) = &args.owner {
        body.insert(
            "owner_id".to_string(),
            json!(resolve_user_id(api, &project, owner)?),
        );
    }
    if let Some(progress) = args.progress {
        body.insert("progress".to_string(), json!(progress));
    }
    if let Some(title) = &args.title {
        body.insert("title".to_string(), json!(title));
    }
    if let Some(description) = &args.description {
        body.insert("description".to_string(), json!(description));
    }
    if let Some(priority) = args.priority {
        body.insert("priority".to_string(), json!(priority));
    }
    if let Some(due_date) = &args.due_date {
        body.insert("due_date".to_string(), json!(due_date));
    }
    if let Some(labels) = &args.labels {
        body.insert("labels".to_string(), json!(labels));
    }
    if let Some(bot_ready) = args.bot_ready {
        body.insert("bot_ready".to_string(), json!(bot_ready));
    }
    if body.is_empty() {
        bail!("nothing to update; pass at least one field option (see --help)");
    }
    let value = api.patch_task(&project, &task_ref, &Value::Object(body))?;
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "updated")
}

pub fn claim(
    api: &Api,
    task_arg: &str,
    project: Option<&str>,
    status_override: Option<&str>,
    if_unassigned: bool,
    json: bool,
) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let task: Task = from_value(api.task_detail(&project, &task_ref)?)?;
    let stages = task_workflow_stages(api, &project, &task.workflow.id)?;
    let status = stages::claim_stage(&stages, status_override)?;
    // atomic claim エンドポイント経由。if_unassigned なら他ユーザー assign 済みは
    // サーバーが 409 を返す (自律エージェントのレース回避)。
    let value = api.claim_task(&project, &task_ref, status, if_unassigned)?;
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "claimed")
}

pub fn complete(
    api: &Api,
    task_arg: &str,
    project: Option<&str>,
    comment: Option<&str>,
    json: bool,
) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let task: Task = from_value(api.task_detail(&project, &task_ref)?)?;
    let stages = task_workflow_stages(api, &project, &task.workflow.id)?;
    let terminal = stages::terminal_stage(&stages)?;
    // 先に status PATCH、成功したらコメント (PATCH 失敗時に完了コメントだけ残るのを防ぐ)。
    // コメント投稿の失敗は警告に留める (完了自体は成功しており、エラー終了すると
    // リトライで二重完了操作になる)。
    let value = api.patch_task(&project, &task_ref, &json!({ "status": terminal.value }))?;
    if let Some(comment) = comment {
        if let Err(error) = api.post_comment(&project, &task_ref, comment) {
            eprintln!("warning: task was completed but posting the comment failed: {error:#}");
        }
    }
    if json {
        return print_json(&value);
    }
    // 終端ステージへの変更で acceptance flow があれば phase=acceptance (検収) になる
    print_task_line(&value, "completed")
}

pub fn comment(
    api: &Api,
    task_arg: &str,
    project: Option<&str>,
    message: &str,
    files: &[PathBuf],
    json: bool,
) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    if message.trim().is_empty() && files.is_empty() {
        bail!("comment message is empty (and no --file given)");
    }
    let value = if files.is_empty() {
        api.post_comment(&project, &task_ref, message)?
    } else {
        api.post_comment_with_files(&project, &task_ref, message, files)?
    };
    if json {
        return print_json(&value);
    }
    println!("commented on {task_arg}");
    Ok(())
}

pub fn events(api: &Api, task_arg: &str, project: Option<&str>, json: bool) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let value = api.events(&project, &task_ref)?;
    if json {
        return print_json(&value);
    }
    let items: Vec<TaskEvent> = from_value(value)?;
    for event in &items {
        let time: String = event.created_at.chars().take(19).collect();
        println!(
            "[{}] {} ({})",
            time.replace('T', " "),
            author_name(&event.author),
            event.event_type
        );
        for line in event.content.lines() {
            println!("  {line}");
        }
        // field_change 等は content が空で metadata.changes に diff が入る
        if let Some(changes) = event.metadata.get("changes").and_then(|c| c.as_array()) {
            for change in changes {
                println!(
                    "  {}: {} → {}",
                    change["field"].as_str().unwrap_or("?"),
                    format_change_value(&change["from"]),
                    format_change_value(&change["to"]),
                );
            }
        }
        for attachment in &event.attachments {
            println!(
                "  attachment: {} ({} bytes)",
                attachment.filename, attachment.file_size
            );
        }
        println!();
    }
    Ok(())
}

pub fn track(api: &Api, task_arg: &str, project: Option<&str>, json: bool) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let value = api.track(&project, &task_ref)?;
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "tracked")
}

pub fn cancel(
    api: &Api,
    task_arg: &str,
    project: Option<&str>,
    reason: &str,
    json: bool,
) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let value = api.cancel(&project, &task_ref, reason)?;
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "cancelled")
}

pub fn resume(api: &Api, task_arg: &str, project: Option<&str>, json: bool) -> Result<()> {
    let (project, task_ref) = resolve_target(task_arg, project)?;
    let value = api.resume(&project, &task_ref)?;
    if json {
        return print_json(&value);
    }
    print_task_line(&value, "resumed")
}
