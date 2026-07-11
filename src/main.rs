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
        /// Only untracked (casual) tasks
        #[arg(long)]
        untracked: bool,
        /// Only tracked tasks
        #[arg(long, conflicts_with = "untracked")]
        tracked: bool,
        /// Max tasks returned (1-500; default 200, or 500 when
        /// --status/--assignee is used)
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Operate on a single task (reference: KEY-N, or UUID with --project)
    #[command(subcommand)]
    Task(TaskCmd),
    /// Allow executing the getter command of a discovered .loadenv.sh
    /// (direnv-style allow; defaults to the nearest candidate)
    Trust { path: Option<PathBuf> },
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
        #[arg(long, conflicts_with_all = ["title", "description", "status", "assignee", "owner", "priority", "due_date", "labels"])]
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
        /// Repeatable; replaces the whole label list
        #[arg(long = "label")]
        labels: Option<Vec<String>>,
    },
    /// Claim a task: assign to me and move to the in-progress stage (対応中)
    Claim {
        task: String,
        #[arg(long)]
        project: Option<String>,
        /// Override the target stage (label or numeric value)
        #[arg(long)]
        status: Option<String>,
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
    // me / orgs は org 不要 (キーがあれば getter を起動しない)
    let need_org = !matches!(cli.command, Cmd::Me | Cmd::Orgs);
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
        Cmd::Tasks {
            project,
            status,
            assignee,
            untracked,
            tracked,
            limit,
        } => commands::tasks(
            &api,
            &project,
            &commands::TasksFilter {
                status,
                assignee,
                untracked,
                tracked,
                limit,
            },
            json,
        ),
        Cmd::Task(task_cmd) => match task_cmd {
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
                labels,
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
                    labels,
                },
                json,
            ),
            TaskCmd::Claim {
                task,
                project,
                status,
            } => commands::claim(&api, &task, project.as_deref(), status.as_deref(), json),
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
    }
}
