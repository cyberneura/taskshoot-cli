mod api;
mod commands;
mod config;
mod models;
mod output;
mod stages;
mod taskref;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Taskshoot task operations CLI.
///
/// Authentication (in order of precedence):
/// 1. TASKSHOOT_API_KEY / TASKSHOOT_CLI_ORGANIZATION environment variables
/// 2. TASKSHOOT_CLI_ENV_GETTER_COMMAND (executed without a shell; stdout is
///    parsed as KEY=VALUE lines)
/// 3. A .loadenv.sh (searched upward from the current directory) exporting
///    TASKSHOOT_CLI_ENV_GETTER_COMMAND
///
/// TASKSHOOT_API_ORIGIN overrides the API origin (default:
/// https://taskshoot-api.cyberneura.com; use http://127.0.0.1:8008 for local dev).
#[derive(Parser)]
#[command(name = "taskshoot", version, about = "Taskshoot task operations CLI")]
struct Cli {
    /// Output raw JSON (for AI agents / scripting)
    #[arg(long, global = true)]
    json: bool,
    /// Organization code name (overrides TASKSHOOT_CLI_ORGANIZATION)
    #[arg(long, global = true)]
    org: Option<String>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the authenticated user (whoami)
    Me,
    /// List organizations the API key can access
    Orgs,
    /// List projects in the organization
    Projects,
    /// List workflows and their stages for a project
    Workflows {
        #[arg(long)]
        project: String,
    },
    /// List task categories for a project
    Categories {
        #[arg(long)]
        project: String,
    },
    /// List tasks in a project
    Tasks {
        #[arg(long)]
        project: String,
        /// Filter by status label (e.g. 起案) or numeric value (server-side)
        #[arg(long)]
        status: Option<String>,
        /// Filter by assignee: "me", handle name, display name, or user id
        /// (server-side)
        #[arg(long)]
        assignee: Option<String>,
        /// Filter to tasks mentioning the user ("me", handle name, display
        /// name, or user id) via @handle in description/comments, including
        /// mention groups the user belongs to (server-side)
        #[arg(long)]
        mentioned: Option<String>,
        /// Only untracked (casual) tasks
        #[arg(long)]
        untracked: bool,
        /// Only tracked tasks
        #[arg(long, conflicts_with = "untracked")]
        tracked: bool,
        /// Filter by Bot Ready flag (true/false); bot loops use --bot-ready true
        #[arg(long)]
        bot_ready: Option<bool>,
        /// Max tasks returned (1-500; default 200, or 500 when
        /// --status/--assignee/--mentioned/--bot-ready is used)
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Search tasks across the organization (hybrid bigram + vector search)
    Search {
        /// Search query (text, or a task ref like KEY-12)
        query: String,
        /// Max results (1-50)
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Operate on a single task (reference: KEY-N, or UUID with --project)
    // TaskCmd は Update/Create 等がフィールド多数で飛び抜けて大きい variant なので、
    // clippy::large_enum_variant を避けるため Box 化して Cmd のサイズ差を抑える
    // (clap は Box<T: Subcommand> の blanket impl を持つのでそのまま動く)。
    #[command(subcommand)]
    Task(Box<TaskCmd>),
    /// List / mark-read your notifications (bot mention inbox; user-scoped)
    #[command(subcommand)]
    Notifications(NotificationsCmd),
    /// Allow executing the getter command of a discovered .loadenv.sh
    /// (direnv-style allow; defaults to the nearest candidate)
    ///
    /// Only relevant when credentials come from a .loadenv.sh (precedence 3).
    /// Setting TASKSHOOT_CLI_ENV_GETTER_COMMAND -- or both TASKSHOOT_API_KEY and
    /// TASKSHOOT_CLI_ORGANIZATION -- skips the .loadenv.sh search entirely, so
    /// trust is unnecessary. Note that TASKSHOOT_API_KEY alone is not enough:
    /// org-scoped commands still search for a .loadenv.sh to resolve the
    /// organization when neither --org nor TASKSHOOT_CLI_ORGANIZATION is given.
    Trust { path: Option<PathBuf> },
}

#[derive(Subcommand)]
enum NotificationsCmd {
    /// List your notifications (newest first) with the unread count
    List {
        /// Max notifications returned (1-100)
        #[arg(long, default_value_t = 30)]
        limit: u32,
        /// Only unread notifications
        #[arg(long)]
        unread_only: bool,
    },
    /// Mark notifications as read (by id, or --all). Prints the updated unread
    /// count. Requires a write API key.
    Read {
        /// Notification ids to mark read (repeatable positional)
        ids: Vec<String>,
        /// Mark all unread notifications as read
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Show task details
    Show {
        task: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Create a task (--title = tracked task / --content = untracked casual task)
    Create {
        #[arg(long)]
        project: String,
        #[arg(long)]
        title: Option<String>,
        /// Create an untracked casual task from this message instead
        /// (tracked-task options cannot be combined)
        #[arg(long, conflicts_with_all = ["title", "description", "status", "assignee", "owner", "priority", "due_date", "labels", "category"])]
        content: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Status label or numeric value
        #[arg(long)]
        status: Option<String>,
        /// "me", handle name, display name, or user id
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        /// YYYY-MM-DD
        #[arg(long)]
        due_date: Option<String>,
        /// Repeatable
        #[arg(long = "label")]
        labels: Vec<String>,
        /// Category name or id (see `taskshoot categories`)
        #[arg(long)]
        category: Option<String>,
    },
    /// Update task fields
    Update {
        task: String,
        #[arg(long)]
        project: Option<String>,
        /// Status label or numeric value
        #[arg(long)]
        status: Option<String>,
        /// "me", handle name, display name, or user id
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        /// 0-100
        #[arg(long)]
        progress: Option<i64>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        priority: Option<i64>,
        /// YYYY-MM-DD
        #[arg(long)]
        due_date: Option<String>,
        /// Start datetime. ISO8601 (e.g. 2026-07-13T10:20:22+09:00 /
        /// `date -Iseconds`). Empty string clears it.
        #[arg(long)]
        started_at: Option<String>,
        /// Completion datetime. ISO8601 (e.g. 2026-07-13T10:20:22+09:00 /
        /// `date -Iseconds`). Empty string clears it.
        #[arg(long)]
        completed_at: Option<String>,
        /// Repeatable; replaces the whole label list
        #[arg(long = "label")]
        labels: Option<Vec<String>>,
        /// Bot が着手してよいか (true/false)
        #[arg(long)]
        bot_ready: Option<bool>,
        /// Category name or id (see `taskshoot categories`). Empty string
        /// clears it.
        #[arg(long)]
        category: Option<String>,
    },
    /// Claim a task: assign to me and move to the in-progress stage (対応中)
    Claim {
        task: String,
        #[arg(long)]
        project: Option<String>,
        /// Override the target stage (label or numeric value)
        #[arg(long)]
        status: Option<String>,
        /// Fail (exit 1, HTTP 409) if the task is already assigned to someone
        /// else. Autonomous bot loops set this to avoid double-processing.
        #[arg(long)]
        if_unassigned: bool,
    },
    /// Complete a task: move to the terminal stage (may enter acceptance phase)
    Complete {
        task: String,
        #[arg(long)]
        project: Option<String>,
        /// Post this comment to the thread after completing
        #[arg(long)]
        comment: Option<String>,
    },
    /// Post a comment to the task thread
    Comment {
        task: String,
        message: String,
        #[arg(long)]
        project: Option<String>,
        /// Attach a file (repeatable)
        #[arg(long = "file")]
        files: Vec<PathBuf>,
    },
    /// Show the task thread (events)
    Events {
        task: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Promote an untracked task to tracked (assigns a task number)
    Track {
        task: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Cancel a task
    Cancel {
        task: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Resume a cancelled task
    Resume {
        task: String,
        #[arg(long)]
        project: Option<String>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // trust は API 設定不要 (config 解決前に処理する)
    if let Cmd::Trust { path } = &cli.command {
        return config::trust_loadenv(path.clone());
    }
    // me / orgs / notifications は org 不要 (通知は user スコープ)
    let need_org = !matches!(cli.command, Cmd::Me | Cmd::Orgs | Cmd::Notifications(_));
    let config = config::resolve(cli.org.clone(), need_org)?;
    let api = api::Api::new(&config)?;
    let json = cli.json;
    match cli.command {
        // Trust は config 解決前に処理済み
        Cmd::Trust { .. } => unreachable!("handled before config resolution"),
        Cmd::Me => commands::me(&api, json),
        Cmd::Orgs => commands::orgs(&api, json),
        Cmd::Projects => commands::projects(&api, json),
        Cmd::Workflows { project } => commands::workflows(&api, &project, json),
        Cmd::Categories { project } => commands::categories(&api, &project, json),
        Cmd::Tasks {
            project,
            status,
            assignee,
            mentioned,
            untracked,
            tracked,
            bot_ready,
            limit,
        } => commands::tasks(
            &api,
            &project,
            &commands::TasksFilter {
                status,
                assignee,
                mentioned,
                untracked,
                tracked,
                bot_ready,
                limit,
            },
            json,
        ),
        Cmd::Search { query, limit } => commands::search(&api, &query, limit, json),
        Cmd::Task(task_cmd) => match *task_cmd {
            TaskCmd::Show { task, project } => {
                commands::show(&api, &task, project.as_deref(), json)
            }
            TaskCmd::Create {
                project,
                title,
                content,
                description,
                status,
                assignee,
                owner,
                priority,
                due_date,
                labels,
                category,
            } => commands::create(
                &api,
                &commands::CreateArgs {
                    project,
                    title,
                    content,
                    description,
                    status,
                    assignee,
                    owner,
                    priority,
                    due_date,
                    labels,
                    category,
                },
                json,
            ),
            TaskCmd::Update {
                task,
                project,
                status,
                assignee,
                owner,
                progress,
                title,
                description,
                priority,
                due_date,
                started_at,
                completed_at,
                labels,
                bot_ready,
                category,
            } => commands::update(
                &api,
                &task,
                project.as_deref(),
                &commands::UpdateArgs {
                    status,
                    assignee,
                    owner,
                    progress,
                    title,
                    description,
                    priority,
                    due_date,
                    started_at,
                    completed_at,
                    labels,
                    bot_ready,
                    category,
                },
                json,
            ),
            TaskCmd::Claim {
                task,
                project,
                status,
                if_unassigned,
            } => commands::claim(
                &api,
                &task,
                project.as_deref(),
                status.as_deref(),
                if_unassigned,
                json,
            ),
            TaskCmd::Complete {
                task,
                project,
                comment,
            } => commands::complete(&api, &task, project.as_deref(), comment.as_deref(), json),
            TaskCmd::Comment {
                task,
                message,
                project,
                files,
            } => commands::comment(&api, &task, project.as_deref(), &message, &files, json),
            TaskCmd::Events { task, project } => {
                commands::events(&api, &task, project.as_deref(), json)
            }
            TaskCmd::Track { task, project } => {
                commands::track(&api, &task, project.as_deref(), json)
            }
            TaskCmd::Cancel {
                task,
                project,
                reason,
            } => commands::cancel(&api, &task, project.as_deref(), &reason, json),
            TaskCmd::Resume { task, project } => {
                commands::resume(&api, &task, project.as_deref(), json)
            }
        },
        Cmd::Notifications(notifications_cmd) => match notifications_cmd {
            NotificationsCmd::List { limit, unread_only } => {
                commands::notifications_list(&api, limit, unread_only, json)
            }
            NotificationsCmd::Read { ids, all } => {
                commands::notifications_read(&api, &ids, all, json)
            }
        },
    }
}
