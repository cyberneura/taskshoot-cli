use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::api::{from_value, Api, TasksQuery};
use crate::models::{
    AssignableUser, Me, MentionCandidate, NotificationList, Org, OrgUser, Project, SearchResult,
    Task, TaskCategory, TaskEvent, Workflow,
};
use crate::output::{print_table, truncate_width};
use crate::stages;
use crate::taskref::{parse_task_ref, TaskRef};

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// From a task argument (KEY-N or UUID), derive the project key and the API task_ref.
/// A slug embeds the key, so it takes precedence over --project. A UUID requires --project.
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

/// Resolve an assignee/owner spec: "me" → yourself, UUID → as-is, otherwise
/// match handle_name / display_name of assignable-users (case-insensitive).
fn resolve_user_id(api: &Api, project: &str, spec: &str) -> Result<String> {
    lookup_user_id(api, project, spec)?
        .with_context(|| format!("no user matched '{spec}' in project {project}"))
}

/// The lookup behind `resolve_user_id`. A spec matching nobody in this project
/// is `Ok(None)` rather than an error, so a multi-project listing can try the
/// next project; an ambiguous spec is still an error (looking elsewhere cannot
/// fix it).
fn lookup_user_id(api: &Api, project: &str, spec: &str) -> Result<Option<String>> {
    if spec == "me" {
        let me: Me = from_value(api.me()?)?;
        return Ok(Some(me.id));
    }
    if Uuid::parse_str(spec).is_ok() {
        return Ok(Some(spec.to_string()));
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
        1 => Ok(Some(matches[0].id.clone())),
        0 => Ok(None),
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
        "{} {}: {} / status: {} ({}) / phase: {} / assignee: {} / category: {}",
        verb,
        task.display_ref(),
        task.title,
        task.status_label,
        task.status,
        task.phase,
        author_name(&task.assignee),
        task.category.as_ref().map_or("-", |c| c.name.as_str()),
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

/// Organization users, taken from the mention candidates: the member list
/// (`members/`) is admin-only, while this endpoint is readable by every role
/// and fills in the same handle names that mentions use.
fn fetch_org_users(api: &Api) -> Result<Vec<OrgUser>> {
    let candidates: Vec<MentionCandidate> = from_value(api.mention_candidates()?)?;
    Ok(candidates
        .iter()
        .filter(|c| c.candidate_type == "user")
        .map(OrgUser::from_candidate)
        .collect())
}

/// Users matching a spec: user id (exact), or handle name / display name
/// (case-insensitive). Returns every match so the caller can report ambiguity
/// instead of picking one arbitrarily.
fn match_org_users<'a>(users: &'a [OrgUser], spec: &str) -> Vec<&'a OrgUser> {
    let spec = spec.trim();
    let needle = spec.to_lowercase();
    users
        .iter()
        .filter(|u| {
            u.id == spec
                || u.display_name.to_lowercase() == needle
                || u.handle_name
                    .as_deref()
                    .is_some_and(|h| h.to_lowercase() == needle)
        })
        .collect()
}

const USER_COLUMNS: [&str; 3] = ["ID", "HANDLE", "NAME"];

fn user_row(user: &OrgUser) -> Vec<String> {
    vec![
        user.id.clone(),
        user.handle_name.clone().unwrap_or_else(|| "-".to_string()),
        user.display_name.clone(),
    ]
}

pub fn users(api: &Api, json: bool) -> Result<()> {
    let users = fetch_org_users(api)?;
    if json {
        return print_json(&serde_json::to_value(&users)?);
    }
    let rows: Vec<Vec<String>> = users.iter().map(user_row).collect();
    print_table(&USER_COLUMNS, &rows);
    Ok(())
}

pub fn user(api: &Api, spec: &str, json: bool) -> Result<()> {
    // "me" cannot be matched against the list: bots have no handle name, and a
    // display name is not necessarily unique. Resolve it to an id first.
    // The trim keeps it consistent with match_org_users, which also trims.
    let spec = if spec.trim() == "me" {
        let me: Me = from_value(api.me()?)?;
        me.id
    } else {
        spec.to_string()
    };
    let users = fetch_org_users(api)?;
    let matches = match_org_users(&users, &spec);
    let user = match matches.len() {
        1 => matches[0],
        0 => bail!("no user matched '{spec}' in this organization"),
        _ => bail!(
            "ambiguous user '{spec}': {}",
            matches
                .iter()
                .map(|u| u.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if json {
        return print_json(&serde_json::to_value(user)?);
    }
    print_table(&USER_COLUMNS, &[user_row(user)]);
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

pub fn categories(api: &Api, project: &str, json: bool) -> Result<()> {
    let value = api.task_categories(project)?;
    if json {
        return print_json(&value);
    }
    let mut items: Vec<TaskCategory> = from_value(value)?;
    items.sort_by_key(|c| c.ordering);
    let rows: Vec<Vec<String>> = items.iter().map(category_row).collect();
    print_table(&CATEGORY_COLUMNS, &rows);
    Ok(())
}

const CATEGORY_COLUMNS: [&str; 5] = ["ID", "NAME", "COLOR", "ORDERING", "STATE"];

fn category_row(category: &TaskCategory) -> Vec<String> {
    vec![
        category.id.clone(),
        category.name.clone(),
        category.color.clone(),
        category.ordering.to_string(),
        if category.active {
            "active"
        } else {
            "inactive"
        }
        .to_string(),
    ]
}

/// Print one category (used after create / update).
fn print_category(value: &Value, json: bool) -> Result<()> {
    if json {
        return print_json(value);
    }
    let category: TaskCategory = from_value(value.clone())?;
    print_table(&CATEGORY_COLUMNS, &[category_row(&category)]);
    Ok(())
}

pub struct CategoryCreateArgs {
    pub project: String,
    pub name: String,
    pub color: Option<String>,
    pub ordering: Option<u32>,
    pub inactive: bool,
}

pub fn category_create(api: &Api, args: &CategoryCreateArgs, json: bool) -> Result<()> {
    let name = args.name.trim();
    if name.is_empty() {
        bail!("--name is empty");
    }
    let mut body = Map::new();
    body.insert("name".to_string(), json!(name));
    if let Some(color) = &args.color {
        body.insert("color".to_string(), json!(color));
    }
    if let Some(ordering) = args.ordering {
        body.insert("ordering".to_string(), json!(ordering));
    }
    if args.inactive {
        body.insert("active".to_string(), json!(false));
    }
    let value = api.create_task_category(&args.project, &Value::Object(body))?;
    print_category(&value, json)
}

pub struct CategoryUpdateArgs {
    pub project: String,
    /// Existing category: name (case-insensitive) or id.
    pub category: String,
    pub name: Option<String>,
    pub color: Option<String>,
    pub ordering: Option<u32>,
    pub active: Option<bool>,
}

pub fn category_update(api: &Api, args: &CategoryUpdateArgs, json: bool) -> Result<()> {
    // Build (and validate) the body before resolving the category, so that a call with
    // no field options fails locally instead of spending a category lookup first.
    let mut body = Map::new();
    if let Some(name) = &args.name {
        let name = name.trim();
        if name.is_empty() {
            bail!("--name is empty");
        }
        body.insert("name".to_string(), json!(name));
    }
    if let Some(color) = &args.color {
        body.insert("color".to_string(), json!(color));
    }
    if let Some(ordering) = args.ordering {
        body.insert("ordering".to_string(), json!(ordering));
    }
    if let Some(active) = args.active {
        body.insert("active".to_string(), json!(active));
    }
    if body.is_empty() {
        bail!("nothing to update: pass --name, --color, --ordering or --active");
    }
    let category_id = resolve_category_id(api, &args.project, &args.category)?
        .context("category is empty; specify a category name or id")?;
    let value = api.patch_task_category(&args.project, &category_id, &Value::Object(body))?;
    print_category(&value, json)
}

/// Resolve a category spec: UUID → as-is, otherwise match a category name in the
/// project (case-insensitive). An empty string means None (clear it).
fn resolve_category_id(api: &Api, project: &str, spec: &str) -> Result<Option<String>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    if Uuid::parse_str(spec).is_ok() {
        return Ok(Some(spec.to_string()));
    }
    let categories: Vec<TaskCategory> = from_value(api.task_categories(project)?)?;
    let needle = spec.to_lowercase();
    let matches: Vec<&TaskCategory> = categories
        .iter()
        .filter(|c| c.name.to_lowercase() == needle)
        .collect();
    match matches.len() {
        1 => Ok(Some(matches[0].id.clone())),
        0 => bail!(
            "unknown category '{spec}' in this project; available: {}",
            categories
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => bail!("category name '{spec}' is ambiguous; specify a category id"),
    }
}

pub struct TasksFilter {
    pub status: Vec<String>,
    pub exclude_status: Vec<String>,
    pub exclude_phase: Vec<String>,
    pub assignee: Option<String>,
    pub mentioned: Option<String>,
    pub mentioned_or_assignee: Option<String>,
    pub untracked: bool,
    pub tracked: bool,
    pub bot_ready: Option<bool>,
    pub limit: Option<u32>,
}

/// A phase (TaskPhase) is accepted as either a label or an english value and is
/// normalized to the english value used by the API. Unlike status it is a fixed
/// enum, so it is resolved locally without hitting the workflow API (keep this in
/// sync with TaskPhase in the backend `task/models.py`).
fn resolve_phase(input: &str) -> Result<&'static str> {
    match input.trim() {
        "pre_approval" | "着手前承認" => Ok("pre_approval"),
        "in_progress" | "進行中" => Ok("in_progress"),
        "acceptance" | "検収" => Ok("acceptance"),
        "done" | "完了" => Ok("done"),
        "rejected" | "却下" => Ok("rejected"),
        "cancelled" | "中止" => Ok("cancelled"),
        "invalid" | "無効" => Ok("invalid"),
        "" => bail!("empty phase value (check for a stray comma)"),
        other => bail!(
            "unknown phase '{other}' (expected one of: 着手前承認/進行中/検収/完了/\
             却下/中止/無効, or their english values pre_approval/in_progress/\
             acceptance/done/rejected/cancelled/invalid)"
        ),
    }
}

/// Resolve multiple phase specs into a list of english values (deduped).
fn resolve_phases(inputs: &[String]) -> Result<Vec<String>> {
    let mut values: Vec<String> = Vec::new();
    for input in inputs {
        let value = resolve_phase(input)?.to_string();
        if !values.contains(&value) {
            values.push(value);
        }
    }
    Ok(values)
}

/// Resolve a status label for list filtering. Looks across all of the project's
/// workflows, and errors if the same label maps to different values across flows
/// (prompting a numeric value instead).
fn resolve_list_status_label(workflows: &[Workflow], input: &str) -> Result<i64> {
    let mut matches: Vec<(i64, String)> = Vec::new();
    for flow in workflows {
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

/// The result of resolving a status filter.
///
/// The server can only filter status by numeric value, so if another workflow
/// uses the same value with a different label, filtering by label still sweeps in
/// tasks of that other label. On the include side these can be dropped by
/// client-side re-filtering via `labels` / `numeric_values`, but on the exclude
/// side the server has already dropped them so they cannot be recovered (we only
/// warn via `collisions`).
#[derive(Default)]
struct ResolvedStatuses {
    /// Status values to send to the server (deduplicated)
    values: Vec<i64>,
    /// The portion specified by label (for re-filtering)
    labels: Vec<String>,
    /// The portion specified directly by number (kept in re-filtering even if the label differs)
    numeric_values: Vec<i64>,
    /// Explanation of when a label-specified value also collides with other labels
    collisions: Vec<String>,
}

impl ResolvedStatuses {
    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Enumerate places where another workflow assigns a different label to the same value.
fn status_value_collisions(workflows: &[Workflow], label: &str, value: i64) -> Vec<String> {
    let mut out = Vec::new();
    for flow in workflows {
        for stage in &flow.stages {
            if stage.active && stage.value == value && stage.label != label {
                out.push(format!(
                    "{label} (={value}) also matches '{}' in {}",
                    stage.label, flow.name
                ));
            }
        }
    }
    out
}

/// Resolve statuses for list filtering (supports multiple values). Only when at
/// least one label is present is the workflow fetched once, and all labels are
/// resolved with it (does not hit the API once per value). Duplicates are folded.
fn resolve_list_statuses(api: &Api, project: &str, inputs: &[String]) -> Result<ResolvedStatuses> {
    if inputs.is_empty() {
        return Ok(ResolvedStatuses::default());
    }
    let inputs: Vec<&str> = inputs.iter().map(|s| s.trim()).collect();
    if inputs.iter().any(|s| s.is_empty()) {
        bail!("empty status value (check for a stray comma)");
    }
    let needs_labels = inputs.iter().any(|s| s.parse::<i64>().is_err());
    let workflows: Vec<Workflow> = if needs_labels {
        from_value(api.project_workflows(project)?)?
    } else {
        Vec::new()
    };
    let mut resolved = ResolvedStatuses::default();
    for input in inputs {
        let value = match input.parse::<i64>() {
            Ok(value) => {
                resolved.numeric_values.push(value);
                value
            }
            Err(_) => {
                let value = resolve_list_status_label(&workflows, input)?;
                resolved.labels.push(input.to_string());
                resolved
                    .collisions
                    .extend(status_value_collisions(&workflows, input, value));
                value
            }
        };
        if !resolved.values.contains(&value) {
            resolved.values.push(value);
        }
    }
    Ok(resolved)
}

/// Normalize the `--project` values: trim, reject blanks, and fold duplicates
/// while keeping the order given (the order decides which project resolves the
/// user filter, and is the tiebreaker of the merged output).
fn normalize_project_keys(projects: &[String]) -> Result<Vec<String>> {
    let mut keys: Vec<String> = Vec::new();
    for project in projects {
        let key = project.trim();
        if key.is_empty() {
            bail!("empty project key (check for a stray comma)");
        }
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }
    if keys.is_empty() {
        bail!("--project requires at least one project key");
    }
    Ok(keys)
}

/// Resolve a user spec once for a multi-project listing.
///
/// `assignable-users` is project-scoped while a user id is organization-wide, so
/// "not a member of the first project" is no reason to fail when the same person
/// is a member of another project being listed: try the projects in order and
/// take the first hit.
///
/// Statuses are *not* resolved this way (see `project_tasks`): a status label
/// belongs to the project's workflows, so it has to be resolved per project.
fn resolve_user_id_for_projects(api: &Api, projects: &[String], spec: &str) -> Result<String> {
    for project in projects {
        if let Some(id) = lookup_user_id(api, project, spec)? {
            return Ok(id);
        }
    }
    match projects {
        [project] => bail!("no user matched '{spec}' in project {project}"),
        _ => bail!(
            "no user matched '{spec}' in projects {}",
            projects.join(", ")
        ),
    }
}

/// Days between 1970-01-01 and the given civil date (Howard Hinnant's
/// `days_from_civil`), so that a timestamp can be reduced to an instant without
/// pulling in a date/time crate.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let shifted_month = (month + 9) % 12; // March = 0
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year; // [0, 146096]
    era * 146097 + day_of_era - 719468
}

/// Split "12:34:56.789+09:00" into its time part and the offset in minutes.
/// A missing offset is taken as UTC (the API always sends one).
fn split_utc_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    // The time itself holds no sign, so the last one starts the offset
    let Some(sign_at) = rest.rfind(['+', '-']) else {
        return Some((rest, 0));
    };
    let (time, offset) = rest.split_at(sign_at);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    // "+09:00" / "+0900" / "+09"
    let digits: Vec<u32> = offset[1..]
        .chars()
        .filter(|c| *c != ':')
        .map(|c| c.to_digit(10))
        .collect::<Option<_>>()?;
    let number = |digits: &[u32]| digits.iter().fold(0i64, |n, d| n * 10 + i64::from(*d));
    let (hours, minutes) = match digits.len() {
        2 => (number(&digits), 0),
        4 => (number(&digits[..2]), number(&digits[2..])),
        _ => return None,
    };
    Some((time, sign * (hours * 60 + minutes)))
}

/// Parse an API timestamp into (seconds since the epoch, microseconds).
///
/// The values are ISO 8601 (`2026-08-05T09:47:08.921505+00:00`), but the exact
/// text varies -- the offset may be written `Z`, and a whole second is sent
/// without the `.000000` -- so comparing the strings would disagree with the
/// chronological order. Parsing is done here rather than with a date crate
/// because ordering a listing is the only thing the CLI needs it for.
fn parse_timestamp(raw: &str) -> Option<(i64, u32)> {
    let (date, rest) = raw.trim().split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (time, offset_minutes) = split_utc_offset(rest)?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let seconds_field = time_parts.next()?;
    if time_parts.next().is_some() {
        return None;
    }
    let (second, fraction) = seconds_field.split_once('.').unwrap_or((seconds_field, ""));
    let second: i64 = second.parse().ok()?;
    // ".9" is 900000 microseconds, so pad on the right (and drop nanoseconds)
    let microseconds = fraction
        .chars()
        .map(|c| c.to_digit(10))
        .chain(std::iter::repeat(Some(0)))
        .take(6)
        .try_fold(0u32, |n, digit| Some(n * 10 + digit?))?;
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - offset_minutes * 60;
    Some((seconds, microseconds))
}

/// Sort key of a listed task: the server orders by `-latest_event_at,
/// -created_at`, so merging several projects on the same key keeps each
/// project's own order intact. A timestamp that cannot be parsed sorts last
/// (there is no sensible instant to place it at).
fn task_sort_key(task: &Value) -> ((i64, u32), (i64, u32)) {
    const OLDEST: (i64, u32) = (i64::MIN, 0);
    let timestamp = |field: &str| {
        task[field]
            .as_str()
            .and_then(parse_timestamp)
            .unwrap_or(OLDEST)
    };
    let created_at = timestamp("created_at");
    let latest_event_at = match task["latest_event_at"].as_str().and_then(parse_timestamp) {
        Some(latest_event_at) => latest_event_at,
        None => created_at,
    };
    (latest_event_at, created_at)
}

/// Order a merged listing the way the server orders a single project's.
fn sort_tasks_newest_first(tasks: &mut [Value]) {
    tasks.sort_by_key(|task| std::cmp::Reverse(task_sort_key(task)));
}

/// Drop tasks already present in the list, keeping the first occurrence. Only
/// needed where the same task can come back from more than one request (a task
/// assigned to a user can also mention them), so the ids are compared as they
/// arrive rather than by re-parsing into a Task.
fn dedupe_tasks_by_id(tasks: &mut Vec<Value>) {
    let mut seen: HashSet<String> = HashSet::new();
    tasks.retain(|task| match task["id"].as_str() {
        // A task with no id cannot be recognized as a duplicate, so keep it
        // rather than silently dropping everything after the first.
        None => true,
        Some(id) => seen.insert(id.to_string()),
    });
}

/// The user filters of one request: the API has no OR between them, so
/// "assignee OR mentioned" is issued as two requests and merged client-side.
#[derive(Clone, Copy)]
struct UserFilters<'a> {
    assignee_id: Option<&'a str>,
    mentioned_user_id: Option<&'a str>,
}

/// The part of a task listing that is resolved once for every project.
struct ProjectTasksQuery<'a> {
    tracked: Option<bool>,
    exclude_phase: &'a [String],
    /// Requests to run per project, OR'd together. Normally one; two when
    /// --mentioned-or-assignee is given.
    user_filters: &'a [UserFilters<'a>],
}

/// List one project's tasks. `multi` only decides whether warnings name the
/// project (they would be ambiguous otherwise).
fn project_tasks(
    api: &Api,
    project: &str,
    filter: &TasksFilter,
    shared: &ProjectTasksQuery,
    multi: bool,
) -> Result<Vec<Value>> {
    // A status label is defined by the project's workflows, so it is resolved
    // per project -- the same label can have a different value elsewhere, and an
    // unknown label is an error rather than a project that silently matches
    // nothing.
    // (--status and --exclude-status are mutually exclusive in clap, so the
    //  workflow is fetched at most once)
    let status = resolve_list_statuses(api, project, &filter.status)?;
    let exclude_status = resolve_list_statuses(api, project, &filter.exclude_status)?;
    let has_user_filter = shared
        .user_filters
        .iter()
        .any(|f| f.assignee_id.is_some() || f.mentioned_user_id.is_some());
    let has_filter = !status.is_empty()
        || !exclude_status.is_empty()
        || !shared.exclude_phase.is_empty()
        || has_user_filter
        || filter.bot_ready.is_some();
    // exclude drops by numeric value on the server, so tasks with a different
    // label sharing that value are swept in too. The client cannot restore them
    // (already absent from the response), so we only warn.
    if !exclude_status.collisions.is_empty() {
        eprintln!(
            "warning: --exclude-status also removed tasks whose label differs but \
             shares the status value: {}",
            exclude_status.collisions.join("; ")
        );
    }
    // The limit is per project: the server applies it before returning, and
    // trimming the merged list afterwards would drop tasks without saying so.
    let limit = filter
        .limit
        .unwrap_or(if has_filter { 500 } else { 200 })
        .clamp(1, 500);
    // Keep raw API fields for --json, so handle it as Value
    let mut items: Vec<Value> = Vec::new();
    let mut truncated = false;
    for user_filter in shared.user_filters {
        let value = api.tasks(
            project,
            &TasksQuery {
                tracked: shared.tracked,
                limit,
                status: status.values.clone(),
                exclude_status: exclude_status.values.clone(),
                exclude_phase: shared.exclude_phase.to_vec(),
                assignee_id: user_filter.assignee_id.map(str::to_string),
                mentioned_user_id: user_filter.mentioned_user_id.map(str::to_string),
                bot_ready: filter.bot_ready,
            },
        )?;
        let mut page: Vec<Value> = from_value(value)?;
        // If exactly `limit` items came back, older matching tasks may have been cut off
        truncated |= page.len() as u32 >= limit;
        items.append(&mut page);
    }
    // The two halves of an OR overlap (a task can both be assigned to the user
    // and mention them), and each half is only sorted within itself.
    if shared.user_filters.len() > 1 {
        dedupe_tasks_by_id(&mut items);
        sort_tasks_newest_first(&mut items);
    }
    if truncated {
        let scope = if multi {
            format!(" of {project}")
        } else {
            String::new()
        };
        eprintln!(
            "warning: result may be truncated to the newest {limit} tasks{scope}; \
             raise --limit (max 500) if you need more"
        );
    }
    // When labels are given, re-filter by exact label match: the server can only
    // filter by number, so if another workflow uses the same value with a
    // different label, those tasks get mixed in. The portion specified directly by
    // number is kept regardless of label (supports mixing labels and numbers).
    if !status.labels.is_empty() {
        items.retain(|t| {
            t["status_label"]
                .as_str()
                .is_some_and(|label| status.labels.iter().any(|l| l == label))
                || t["status"]
                    .as_i64()
                    .is_some_and(|value| status.numeric_values.contains(&value))
        });
    }
    Ok(items)
}

pub fn tasks(api: &Api, projects: &[String], filter: &TasksFilter, json: bool) -> Result<()> {
    let projects = normalize_project_keys(projects)?;
    let tracked = if filter.untracked {
        Some(false)
    } else if filter.tracked {
        Some(true)
    } else {
        None
    };
    let exclude_phase = resolve_phases(&filter.exclude_phase)?;
    // A user id is organization-wide, so it is resolved once and reused for
    // every project (a status label is not: see the per-project loop below)
    let assignee_id = match &filter.assignee {
        Some(assignee) => Some(resolve_user_id_for_projects(api, &projects, assignee)?),
        None => None,
    };
    let mentioned_user_id = match &filter.mentioned {
        Some(mentioned) => Some(resolve_user_id_for_projects(api, &projects, mentioned)?),
        None => None,
    };
    // --mentioned-or-assignee is the union of the two filters above. The API
    // applies its filters with AND only, so it is sent as two requests whose
    // results are merged per project (see project_tasks).
    let mentioned_or_assignee_id = match &filter.mentioned_or_assignee {
        Some(user) => Some(resolve_user_id_for_projects(api, &projects, user)?),
        None => None,
    };
    let user_filters = match mentioned_or_assignee_id.as_deref() {
        Some(user_id) => vec![
            UserFilters {
                assignee_id: Some(user_id),
                mentioned_user_id: None,
            },
            UserFilters {
                assignee_id: None,
                mentioned_user_id: Some(user_id),
            },
        ],
        None => vec![UserFilters {
            assignee_id: assignee_id.as_deref(),
            mentioned_user_id: mentioned_user_id.as_deref(),
        }],
    };
    let multi = projects.len() > 1;
    let mut items: Vec<Value> = Vec::new();
    for project in &projects {
        let mut project_items = project_tasks(
            api,
            project,
            filter,
            &ProjectTasksQuery {
                tracked,
                exclude_phase: &exclude_phase,
                user_filters: &user_filters,
            },
            multi,
        )
        .with_context(|| format!("project {project}"))?;
        items.append(&mut project_items);
    }
    // Each project comes back sorted, so the merged list is re-sorted on the
    // server's own key to read as one list (a single project keeps its order)
    if multi {
        sort_tasks_newest_first(&mut items);
    }

    if json {
        return print_json(&Value::Array(items));
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            let task: Task = from_value(item.clone())?;
            Ok(task_row(&task, multi))
        })
        .collect::<Result<_>>()?;
    let headers: &[&str] = if multi {
        &[
            "REF", "PROJECT", "STATUS", "PROG", "ASSIGNEE", "TITLE", "UPDATED",
        ]
    } else {
        &["REF", "STATUS", "PROG", "ASSIGNEE", "TITLE", "UPDATED"]
    };
    print_table(headers, &rows);
    Ok(())
}

/// One table row for `tasks`. With several projects merged into one list, a
/// PROJECT column is added: an untracked task has no number, so its ref is a
/// bare UUID, and `task show <uuid>` needs the project key that the ref alone
/// no longer implies.
fn task_row(task: &Task, multi: bool) -> Vec<String> {
    let updated = task
        .latest_event_at
        .as_deref()
        .unwrap_or(&task.created_at)
        .chars()
        .take(10)
        .collect::<String>();
    let mut row = vec![task.display_ref()];
    if multi {
        row.push(task.project_key.clone());
    }
    row.extend([
        task.status_label.clone(),
        format!("{}%", task.progress),
        author_name(&task.assignee),
        truncate_width(&task.title, 48),
        updated,
    ]);
    row
}

pub fn search(api: &Api, query: &str, limit: u32, json: bool) -> Result<()> {
    let value = api.search_tasks(query, limit.clamp(1, 50))?;
    if json {
        return print_json(&value);
    }
    let items: Vec<SearchResult> = from_value(value)?;
    if items.is_empty() {
        println!("no tasks matched");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.display_ref(),
                item.status_label.clone(),
                truncate_width(&item.title, 64),
            ]
        })
        .collect();
    print_table(&["REF", "STATUS", "TITLE"], &rows);
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
    if let Some(category) = &task.category {
        println!("category: {}", category.name);
    }
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
    pub category: Option<String>,
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
    if let Some(category) = &args.category {
        if let Some(category_id) = resolve_category_id(api, &args.project, category)? {
            body.insert("category_id".to_string(), json!(category_id));
        }
    }
    let mut value = api.create_task_full(&args.project, &Value::Object(body))?;
    // The create API's init_lifecycle resets status to the initial stage, so a
    // --status value is applied via a PATCH after creation.
    // Even if the PATCH fails, creation itself succeeded, so we do not error out
    // and only warn (erroring would create a duplicate task on retry).
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
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub labels: Option<Vec<String>>,
    pub bot_ready: Option<bool>,
    pub category: Option<String>,
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
            // Resolve the label against the stage set of the task's workflow
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
    // ISO8601 datetime. An empty string is interpreted as None (clear) by the server's _parse_datetime.
    if let Some(started_at) = &args.started_at {
        body.insert("started_at".to_string(), json!(started_at));
    }
    if let Some(completed_at) = &args.completed_at {
        body.insert("completed_at".to_string(), json!(completed_at));
    }
    if let Some(labels) = &args.labels {
        body.insert("labels".to_string(), json!(labels));
    }
    if let Some(bot_ready) = args.bot_ready {
        body.insert("bot_ready".to_string(), json!(bot_ready));
    }
    // An empty string is sent as null (clears the category).
    if let Some(category) = &args.category {
        body.insert(
            "category_id".to_string(),
            json!(resolve_category_id(api, &project, category)?),
        );
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
    // Via the atomic claim endpoint. With if_unassigned, if the task is already
    // assigned to another user the server returns 409 (avoids autonomous-agent races).
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
    // PATCH the status first, then comment on success (prevents leaving only a
    // completion comment when the PATCH fails). A failed comment post is only
    // warned (completion itself succeeded; erroring out would double the
    // completion operation on retry).
    let value = api.patch_task(&project, &task_ref, &json!({ "status": terminal.value }))?;
    if let Some(comment) = comment {
        if let Err(error) = api.post_comment(&project, &task_ref, comment) {
            eprintln!("warning: task was completed but posting the comment failed: {error:#}");
        }
    }
    if json {
        return print_json(&value);
    }
    // Changing to the terminal stage moves phase=acceptance (検収) if an acceptance flow exists
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
        // For events like field_change, content is empty and the diff is in metadata.changes
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

pub fn notifications_list(api: &Api, limit: u32, unread_only: bool, json: bool) -> Result<()> {
    let value = api.notifications(limit.clamp(1, 100), unread_only)?;
    if json {
        return print_json(&value);
    }
    let list: NotificationList = from_value(value)?;
    println!("unread: {}", list.unread_count);
    if list.items.is_empty() {
        println!("no notifications");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = list
        .items
        .iter()
        .map(|n| {
            let when: String = n.created_at.chars().take(10).collect();
            vec![
                n.id.clone(),
                if n.read {
                    String::new()
                } else {
                    "*".to_string()
                },
                n.notification_type.clone(),
                n.task_ref(),
                truncate_width(&n.body, 60),
                when,
            ]
        })
        .collect();
    print_table(&["ID", "NEW", "TYPE", "TASK", "BODY", "WHEN"], &rows);
    Ok(())
}

pub fn notifications_read(api: &Api, ids: &[String], all: bool, json: bool) -> Result<()> {
    if !all && ids.is_empty() {
        bail!("pass notification ids to mark read, or --all");
    }
    let value = api.mark_notifications_read(ids, all)?;
    if json {
        return print_json(&value);
    }
    let unread = value
        .get("unread_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    println!("marked as read; unread now: {unread}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUZUKI_ID: &str = "019f31d1-0000-0000-0000-000000000001";
    const BOT_ID: &str = "019f31d1-0000-0000-0000-000000000002";

    fn mention_candidates() -> Vec<MentionCandidate> {
        from_value(json!([
            {
                "type": "user",
                "id": SUZUKI_ID,
                "handle_name": "Suzuki",
                "display_name": "Suzuki Taro",
            },
            // Bots have no email, so the server cannot derive a handle name.
            {
                "type": "user",
                "id": BOT_ID,
                "handle_name": "",
                "display_name": "Amedeo",
            },
            {
                "type": "group",
                "id": "019f31d1-0000-0000-0000-000000000003",
                "handle_name": "dev-team",
                "display_name": "Dev team",
            },
        ]))
        .unwrap()
    }

    fn org_users() -> Vec<OrgUser> {
        mention_candidates()
            .iter()
            .filter(|c| c.candidate_type == "user")
            .map(OrgUser::from_candidate)
            .collect()
    }

    fn matched_ids(spec: &str) -> Vec<String> {
        match_org_users(&org_users(), spec)
            .iter()
            .map(|u| u.id.clone())
            .collect()
    }

    #[test]
    fn matches_user_by_handle_display_name_and_id() {
        for spec in ["suzuki", "SUZUKI", "Suzuki Taro", " suzuki "] {
            assert_eq!(
                matched_ids(spec),
                vec![SUZUKI_ID.to_string()],
                "spec {spec}"
            );
        }
        assert_eq!(matched_ids(SUZUKI_ID), vec![SUZUKI_ID.to_string()]);
    }

    #[test]
    fn matches_a_bot_without_a_handle_by_display_name() {
        assert_eq!(matched_ids("amedeo"), vec![BOT_ID.to_string()]);
    }

    #[test]
    fn an_empty_handle_name_matches_nothing() {
        assert_eq!(
            OrgUser::from_candidate(&mention_candidates()[1]).handle_name,
            None
        );
        assert!(matched_ids("").is_empty());
    }

    #[test]
    fn mention_groups_are_not_users() {
        assert!(matched_ids("dev-team").is_empty());
    }

    fn workflows(spec: &[(&str, &[(i64, &str)])]) -> Vec<Workflow> {
        let value = json!(spec
            .iter()
            .map(|(name, stages)| json!({
                "name": name,
                "stages": stages
                    .iter()
                    .map(|(value, label)| json!({"value": value, "label": label}))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>());
        from_value(value).unwrap()
    }

    #[test]
    fn resolves_label_from_single_workflow() {
        let flows = workflows(&[("default", &[(10, "起案"), (40, "対応中")])]);
        assert_eq!(resolve_list_status_label(&flows, "対応中").unwrap(), 40);
    }

    #[test]
    fn same_label_same_value_across_workflows_is_not_ambiguous() {
        let flows = workflows(&[
            ("default", &[(40, "対応中")]),
            ("review", &[(40, "対応中"), (50, "検収中")]),
        ]);
        assert_eq!(resolve_list_status_label(&flows, "対応中").unwrap(), 40);
    }

    #[test]
    fn same_label_different_values_is_an_error() {
        let flows = workflows(&[
            ("default", &[(40, "対応中")]),
            ("review", &[(45, "対応中")]),
        ]);
        assert!(resolve_list_status_label(&flows, "対応中").is_err());
    }

    #[test]
    fn unknown_label_is_an_error() {
        let flows = workflows(&[("default", &[(10, "起案")])]);
        assert!(resolve_list_status_label(&flows, "no-such-label").is_err());
    }

    #[test]
    fn inactive_stage_is_not_matched() {
        let value = json!([{
            "name": "default",
            "stages": [{"value": 10, "label": "起案", "active": false}],
        }]);
        let flows: Vec<Workflow> = from_value(value).unwrap();
        assert!(resolve_list_status_label(&flows, "起案").is_err());
    }

    #[test]
    fn detects_same_value_used_by_a_different_label() {
        // Another workflow assigns a different label to 40: exclude sweeps them in
        let flows = workflows(&[
            ("default", &[(40, "対応中")]),
            ("review", &[(40, "レビュー中")]),
        ]);
        let collisions = status_value_collisions(&flows, "対応中", 40);
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].contains("レビュー中"));
        assert!(collisions[0].contains("review"));
    }

    #[test]
    fn no_collision_when_value_is_used_consistently() {
        let flows = workflows(&[
            ("default", &[(40, "対応中")]),
            ("review", &[(40, "対応中")]),
        ]);
        assert!(status_value_collisions(&flows, "対応中", 40).is_empty());
    }

    #[test]
    fn resolves_phase_from_label_and_english_value() {
        assert_eq!(resolve_phase("無効").unwrap(), "invalid");
        assert_eq!(resolve_phase("完了").unwrap(), "done");
        assert_eq!(resolve_phase("invalid").unwrap(), "invalid");
        assert_eq!(resolve_phase(" 進行中 ").unwrap(), "in_progress");
    }

    #[test]
    fn unknown_phase_is_an_error() {
        // 起案 is a status, not a phase
        assert!(resolve_phase("起案").is_err());
        assert!(resolve_phase("bogus").is_err());
        assert!(resolve_phase("").is_err());
    }

    #[test]
    fn resolve_phases_dedupes_across_label_and_value() {
        // "無効" and "invalid" fold into the same value
        let out = resolve_phases(&["完了".into(), "無効".into(), "invalid".into()]).unwrap();
        assert_eq!(out, vec!["done".to_string(), "invalid".to_string()]);
    }

    fn project_keys(input: &[&str]) -> Result<Vec<String>> {
        normalize_project_keys(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn project_keys_are_trimmed_and_deduped_in_order() {
        assert_eq!(
            project_keys(&["DEV", " SALES ", "DEV"]).unwrap(),
            ["DEV", "SALES"]
        );
    }

    #[test]
    fn a_blank_project_key_is_an_error() {
        // "--project DEV,,SALES" splits into an empty key
        assert!(project_keys(&["DEV", "", "SALES"]).is_err());
        assert!(project_keys(&["  "]).is_err());
        assert!(project_keys(&[]).is_err());
    }

    fn task_json(latest_event_at: Option<&str>, created_at: &str) -> Value {
        json!({"latest_event_at": latest_event_at, "created_at": created_at})
    }

    #[test]
    fn merged_tasks_sort_newest_first_across_projects() {
        // as returned by two projects, each already newest-first
        let mut items = [
            task_json(
                Some("2026-08-05T09:00:00+00:00"),
                "2026-08-01T00:00:00+00:00",
            ),
            task_json(
                Some("2026-08-03T09:00:00+00:00"),
                "2026-07-01T00:00:00+00:00",
            ),
            task_json(
                Some("2026-08-04T09:00:00+00:00"),
                "2026-08-02T00:00:00+00:00",
            ),
        ];
        sort_tasks_newest_first(&mut items);
        let order: Vec<&str> = items
            .iter()
            .map(|t| t["latest_event_at"].as_str().unwrap())
            .collect();
        assert_eq!(
            order,
            [
                "2026-08-05T09:00:00+00:00",
                "2026-08-04T09:00:00+00:00",
                "2026-08-03T09:00:00+00:00",
            ]
        );
    }

    #[test]
    fn a_task_without_events_sorts_by_its_creation_time() {
        let no_events = task_json(None, "2026-08-04T00:00:00+00:00");
        let older = task_json(
            Some("2026-08-03T00:00:00+00:00"),
            "2026-08-01T00:00:00+00:00",
        );
        assert!(task_sort_key(&no_events) > task_sort_key(&older));
    }

    #[test]
    fn an_unparsable_timestamp_sorts_last() {
        let broken = task_json(Some("not a timestamp"), "also not a timestamp");
        let ancient = task_json(
            Some("1970-01-01T00:00:00+00:00"),
            "1970-01-01T00:00:00+00:00",
        );
        assert!(task_sort_key(&broken) < task_sort_key(&ancient));
    }

    #[test]
    fn timestamps_parse_to_the_same_instant_in_every_written_form() {
        let epoch = Some((0, 0));
        assert_eq!(parse_timestamp("1970-01-01T00:00:00+00:00"), epoch);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), epoch);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00.000000Z"), epoch);
        assert_eq!(parse_timestamp("1970-01-01T00:00:00"), epoch);
        // the same instant written in three other zones
        assert_eq!(parse_timestamp("1970-01-01T09:00:00+09:00"), epoch);
        assert_eq!(parse_timestamp("1970-01-01T09:00:00+0900"), epoch);
        assert_eq!(parse_timestamp("1969-12-31T19:00:00-05:00"), epoch);
        // a leading zero of the fraction counts: ".05" is 50000 microseconds
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:00.05Z"),
            Some((0, 50_000))
        );
        // nanosecond precision is truncated, not misread
        assert_eq!(
            parse_timestamp("1970-01-01T00:00:00.123456789Z"),
            Some((0, 123_456))
        );
        // a date the leap-year rules have to get right
        assert_eq!(
            parse_timestamp("2000-02-29T00:00:00Z"),
            Some((951_782_400, 0))
        );
    }

    #[test]
    fn a_mixed_form_timestamp_still_sorts_chronologically() {
        // "+00:00" > "Z" and "0" < "9" as text, so a text comparison would put
        // these two backwards
        let with_offset = task_json(
            Some("2026-08-05T00:00:00.000001+00:00"),
            "2026-08-01T00:00:00Z",
        );
        let with_z = task_json(Some("2026-08-05T00:00:00.9Z"), "2026-08-01T00:00:00Z");
        assert!(task_sort_key(&with_z) > task_sort_key(&with_offset));
    }

    #[test]
    fn a_malformed_timestamp_is_rejected_rather_than_guessed() {
        for raw in [
            "",
            "2026-08-05",
            "2026-08-05T09:00",
            "2026-08-05T09:00:00:00Z",
            "2026-13-05T09:00:00Z",
            "2026-08-05T09:00:0a Z",
            "2026-08-05T09:00:00+9",
            "2026-08-05T09:00:00.１２Z",
        ] {
            assert_eq!(parse_timestamp(raw), None, "{raw} should not parse");
        }
    }

    fn row_task(number: Option<u64>) -> Task {
        from_value(json!({
            "id": "019f31d1-0000-0000-0000-0000000000ff",
            "project_key": "SALES",
            "number": number,
            "title": "見積もりを送る",
            "workflow": {"id": "019f31d1-0000-0000-0000-000000000010", "name": "デフォルト"},
            "phase": "in_progress",
            "status": 40,
            "status_label": "対応中",
            "progress": 30,
            "assignee": null,
            "owner": null,
            "reporter": null,
            "tracked": number.is_some(),
            "latest_event_at": "2026-08-05T09:00:00Z",
            "created_at": "2026-08-01T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn a_single_project_row_has_no_project_column() {
        let row = task_row(&row_task(Some(12)), false);
        assert_eq!(row[0], "SALES-12");
        assert_eq!(row[1], "対応中");
    }

    #[test]
    fn a_merged_row_names_its_project() {
        let row = task_row(&row_task(Some(12)), true);
        assert_eq!(row[0], "SALES-12");
        assert_eq!(row[1], "SALES");
        assert_eq!(row[2], "対応中");
    }

    #[test]
    fn an_untracked_merged_row_names_the_project_its_uuid_needs() {
        // an untracked task has no number, so its ref is a bare UUID and
        // "task show <uuid>" cannot tell which --project to pass
        let row = task_row(&row_task(None), true);
        assert_eq!(row[0], "019f31d1-0000-0000-0000-0000000000ff");
        assert_eq!(row[1], "SALES");
    }

    fn ids_of(tasks: &[Value]) -> Vec<&str> {
        tasks
            .iter()
            .map(|t| t["id"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn merging_an_or_keeps_the_first_copy_of_a_task_matched_twice() {
        // the assigned half and the mentioned half of --mentioned-or-assignee
        // both return a task that is assigned to the user and mentions them
        let mut tasks = vec![
            json!({"id": "a"}),
            json!({"id": "b"}),
            json!({"id": "a"}),
            json!({"id": "c"}),
        ];
        dedupe_tasks_by_id(&mut tasks);
        assert_eq!(ids_of(&tasks), ["a", "b", "c"]);
    }

    #[test]
    fn merging_keeps_rows_that_carry_no_id() {
        // an unrecognizable row is kept rather than folded into one another
        let mut tasks = vec![json!({}), json!({"id": "a"}), json!({})];
        dedupe_tasks_by_id(&mut tasks);
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn a_merged_or_listing_is_ordered_newest_first() {
        // each half comes back sorted on its own, so the union is re-sorted on
        // the server's key (-latest_event_at, -created_at)
        let mut tasks = vec![
            json!({"id": "old", "latest_event_at": "2026-08-01T00:00:00Z",
                   "created_at": "2026-07-01T00:00:00Z"}),
            json!({"id": "new", "latest_event_at": "2026-08-05T09:00:00Z",
                   "created_at": "2026-07-02T00:00:00Z"}),
            // no event yet: created_at stands in for latest_event_at
            json!({"id": "mid", "latest_event_at": null,
                   "created_at": "2026-08-03T00:00:00Z"}),
        ];
        sort_tasks_newest_first(&mut tasks);
        assert_eq!(ids_of(&tasks), ["new", "mid", "old"]);
    }
}
