mod api;
mod commands;
mod config;
mod models;
mod output;
mod stages;
mod taskref;

use std::path::PathBuf;

use anyhow::Result;
use clap::builder::RangedI64ValueParser;
use clap::{Parser, Subcommand};

/// Taskshoot task operations CLI.
///
/// Authentication (in order of precedence):
/// 1. Command line flags (--org)
/// 2. TASKSHOOT_CLI_API_KEY / TASKSHOOT_CLI_ORGANIZATION environment variables
/// 3. ~/.config/taskshoot/config.yml (see `taskshoot config init`), with the
///    YAML printed by its config_override_command merged over it
///
/// TASKSHOOT_CLI_API_ORIGIN, or api_origin in the config file, overrides the API
/// origin (default: https://taskshoot-api.cyberneura.com; use
/// http://127.0.0.1:8008 for local dev).
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
    /// List users in the organization (id / handle name / display name)
    Users,
    /// Show one user: "me", a handle name, a display name, or a user id
    User { user: String },
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
    /// Create / update a task category (requires the manager role or higher)
    #[command(subcommand)]
    Category(CategoryCmd),
    /// List tasks in a project (or in several, merged). Without --project,
    /// every non-archived project of the organization is listed
    Tasks {
        /// Project key. Repeatable and comma-separated; multiple projects are
        /// OR'd into one list (e.g. --project GENERAL,SALES). Omit it to cover
        /// every non-archived project (see --include-archived-projects), in
        /// which case a project whose workflows do not define the status label
        /// is skipped with a warning instead of failing the command. --limit
        /// then applies per project
        #[arg(long, value_delimiter = ',')]
        project: Vec<String>,
        /// Filter by status label (e.g. 起案) or numeric value (server-side).
        /// Repeatable and comma-separated; multiple values are OR'd
        /// (e.g. --status 起案,対応中)
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// Exclude these statuses (label or numeric; repeatable and
        /// comma-separated). Cannot be combined with --status
        #[arg(long, value_delimiter = ',', conflicts_with = "status")]
        exclude_status: Vec<String>,
        /// Exclude tasks in these lifecycle phases (label or english value;
        /// repeatable and comma-separated). phase is independent of status --
        /// e.g. "無効" (invalid) keeps its status, so only this excludes it.
        /// Values: 進行中/in_progress 完了/done 無効/invalid 却下/rejected
        /// 中止/cancelled 検収/acceptance 着手前承認/pre_approval
        #[arg(long, value_delimiter = ',')]
        exclude_phase: Vec<String>,
        /// Filter by assignee: "me", handle name, display name, or user id
        /// (server-side)
        #[arg(long)]
        assignee: Option<String>,
        /// Filter to tasks mentioning the user ("me", handle name, display
        /// name, or user id) via @handle in description/comments, including
        /// mention groups the user belongs to (server-side)
        #[arg(long)]
        mentioned: Option<String>,
        /// Filter to tasks assigned to the user OR mentioning them (the union
        /// of --assignee and --mentioned; bot loops use
        /// --mentioned-or-assignee me). Sent as two requests per project and
        /// merged, so --limit applies to each half
        #[arg(long, conflicts_with_all = ["assignee", "mentioned"])]
        mentioned_or_assignee: Option<String>,
        /// Only untracked (casual) tasks
        #[arg(long)]
        untracked: bool,
        /// Only tracked tasks
        #[arg(long, conflicts_with = "untracked")]
        tracked: bool,
        /// Filter by Bot Ready flag (true/false); bot loops use --bot-ready true
        #[arg(long)]
        bot_ready: Option<bool>,
        /// Also sweep archived projects, which are skipped by default. Rejected
        /// together with --project, which always lists the project it names,
        /// archived or not
        #[arg(long, conflicts_with = "project")]
        include_archived_projects: bool,
        /// Max tasks returned per project and per request (1-500; default 200,
        /// or 500 when a status, phase, assignee, mentioned,
        /// mentioned-or-assignee or bot-ready filter is used)
        #[arg(long)]
        limit: Option<u32>,
        /// Print only how many tasks matched, instead of the list. Prints the
        /// bare number, or {"count": N} with --json. Useful to decide whether
        /// there is any work before handing the list to an AI agent. The count
        /// is what the same command would list, so it is capped by --limit
        /// (a truncated result still warns on stderr)
        #[arg(long)]
        count: bool,
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
    // TaskCmd is by far the largest variant (Update/Create etc. have many fields),
    // so it is Boxed to keep Cmd's size difference small and avoid
    // clippy::large_enum_variant (clap has a blanket impl for Box<T: Subcommand>,
    // so it works as-is).
    #[command(subcommand)]
    Task(Box<TaskCmd>),
    /// List / mark-read your notifications (bot mention inbox; user-scoped)
    #[command(subcommand)]
    Notifications(NotificationsCmd),
    /// Inspect or create the config file (~/.config/taskshoot/config.yml)
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the path of the config file that will be read
    Path,
    /// Create a template config file (readable only by you) if none exists
    Init,
    /// Print the merged configuration, running config_override_command.
    /// The API key is masked.
    Show,
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

/// The server stores `ordering` in a PositiveIntegerField, i.e. a PostgreSQL `integer`
/// with a `>= 0` check, so anything above 2^31-1 fails in the database rather than
/// in the API layer.
///
/// The parser has to be built from `value_parser!(u32)` so that it yields the same
/// type as the field it fills: clap does not convert between a parser's output type
/// and the declared field type, it downcasts, and a mismatch panics at parse time.
fn ordering_value_parser() -> RangedI64ValueParser<u32> {
    clap::value_parser!(u32).range(0..=2_147_483_647)
}

#[derive(Subcommand)]
enum CategoryCmd {
    /// Create a task category in a project
    Create {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
        /// Free-form color label used by the web UI (e.g. red)
        #[arg(long)]
        color: Option<String>,
        /// Sort order in the category list (smaller comes first)
        #[arg(long, value_parser = ordering_value_parser())]
        ordering: Option<u32>,
        /// Create it hidden from the task form (can be re-enabled with
        /// `category update --active true`)
        #[arg(long)]
        inactive: bool,
    },
    /// Update a task category (name, color, ordering or active state)
    Update {
        /// Existing category: name (case-insensitive) or id
        category: String,
        #[arg(long)]
        project: String,
        /// New name
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        color: Option<String>,
        #[arg(long, value_parser = ordering_value_parser())]
        ordering: Option<u32>,
        /// Show (true) or hide (false) the category in the task form.
        /// Hiding keeps it on the tasks that already use it.
        #[arg(long)]
        active: Option<bool>,
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
        /// Whether a bot may start working on this task (true/false)
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
    config::warn_about_renamed_env();
    // The config subcommand must work before credentials resolve, since it is
    // what you reach for when they do not.
    if let Cmd::Config(config_cmd) = &cli.command {
        return match config_cmd {
            ConfigCmd::Path => config::print_config_path(),
            ConfigCmd::Init => config::init_config(),
            ConfigCmd::Show => config::show_config(cli.json),
        };
    }
    // me / orgs / notifications need no org (notifications are user-scoped)
    let need_org = !matches!(cli.command, Cmd::Me | Cmd::Orgs | Cmd::Notifications(_));
    let config = config::resolve(cli.org.clone(), need_org)?;
    let api = api::Api::new(&config)?;
    let json = cli.json;
    match cli.command {
        // Config was already handled before credential resolution
        Cmd::Config(_) => unreachable!("handled before config resolution"),
        Cmd::Me => commands::me(&api, json),
        Cmd::Orgs => commands::orgs(&api, json),
        Cmd::Projects => commands::projects(&api, json),
        Cmd::Users => commands::users(&api, json),
        Cmd::User { user } => commands::user(&api, &user, json),
        Cmd::Workflows { project } => commands::workflows(&api, &project, json),
        Cmd::Categories { project } => commands::categories(&api, &project, json),
        Cmd::Category(cmd) => match cmd {
            CategoryCmd::Create {
                project,
                name,
                color,
                ordering,
                inactive,
            } => commands::category_create(
                &api,
                &commands::CategoryCreateArgs {
                    project,
                    name,
                    color,
                    ordering,
                    inactive,
                },
                json,
            ),
            CategoryCmd::Update {
                category,
                project,
                name,
                color,
                ordering,
                active,
            } => commands::category_update(
                &api,
                &commands::CategoryUpdateArgs {
                    project,
                    category,
                    name,
                    color,
                    ordering,
                    active,
                },
                json,
            ),
        },
        Cmd::Tasks {
            project,
            status,
            exclude_status,
            exclude_phase,
            assignee,
            mentioned,
            mentioned_or_assignee,
            untracked,
            tracked,
            bot_ready,
            include_archived_projects,
            limit,
            count,
        } => commands::tasks(
            &api,
            &commands::ProjectScope {
                projects: project,
                include_archived: include_archived_projects,
            },
            &commands::TasksFilter {
                status,
                exclude_status,
                exclude_phase,
                assignee,
                mentioned,
                mentioned_or_assignee,
                untracked,
                tracked,
                bot_ready,
                limit,
            },
            // json and count are both output modes; a struct keeps them from
            // being swapped at the call site
            commands::TasksOutput { json, count },
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn ordering_of(args: &[&str]) -> Option<u32> {
        // Extracting the value is the point: a value_parser whose output type does not
        // match the field type only panics here, not at definition time.
        match Cli::try_parse_from(args).expect("should parse").command {
            Cmd::Category(CategoryCmd::Create { ordering, .. })
            | Cmd::Category(CategoryCmd::Update { ordering, .. }) => ordering,
            _ => panic!("expected a category command"),
        }
    }

    #[test]
    fn category_ordering_parses_in_range_values() {
        assert_eq!(
            ordering_of(&[
                "taskshoot",
                "category",
                "create",
                "--project",
                "DEV",
                "--name",
                "Bug",
                "--ordering",
                "5",
            ]),
            Some(5)
        );
        assert_eq!(
            ordering_of(&[
                "taskshoot",
                "category",
                "update",
                "Bug",
                "--project",
                "DEV",
                "--ordering",
                "2147483647",
            ]),
            Some(2_147_483_647)
        );
    }

    #[test]
    fn category_ordering_rejects_out_of_range_values() {
        for value in ["-1", "2147483648"] {
            assert!(
                Cli::try_parse_from([
                    "taskshoot",
                    "category",
                    "create",
                    "--project",
                    "DEV",
                    "--name",
                    "Bug",
                    &format!("--ordering={value}"),
                ])
                .is_err(),
                "--ordering={value} should be rejected"
            );
        }
    }

    fn tasks_projects_of(args: &[&str]) -> Vec<String> {
        match Cli::try_parse_from(args).expect("should parse").command {
            Cmd::Tasks { project, .. } => project,
            _ => panic!("expected the tasks command"),
        }
    }

    #[test]
    fn tasks_project_accepts_comma_separated_and_repeated_keys() {
        assert_eq!(
            tasks_projects_of(&["taskshoot", "tasks", "--project", "DEV"]),
            ["DEV"]
        );
        assert_eq!(
            tasks_projects_of(&["taskshoot", "tasks", "--project", "DEV,SALES"]),
            ["DEV", "SALES"]
        );
        assert_eq!(
            tasks_projects_of(&[
                "taskshoot",
                "tasks",
                "--project",
                "DEV",
                "--project",
                "SALES,QA",
            ]),
            ["DEV", "SALES", "QA"]
        );
    }

    #[test]
    fn tasks_without_a_project_parses_to_an_empty_list() {
        // no key means "every project"; commands::tasks fills the list in
        assert!(tasks_projects_of(&["taskshoot", "tasks"]).is_empty());
        // and the other filters still apply, as this is the bot loop's form
        assert!(tasks_projects_of(&[
            "taskshoot",
            "tasks",
            "--bot-ready",
            "true",
            "--status",
            "起案"
        ])
        .is_empty());
    }

    #[test]
    fn tasks_mentioned_or_assignee_replaces_the_two_filters_it_unions() {
        // it means "--assignee OR --mentioned", so combining it with either of
        // them would be an AND of a filter with its own union
        for other in ["--assignee", "--mentioned"] {
            assert!(
                Cli::try_parse_from([
                    "taskshoot",
                    "tasks",
                    "--project",
                    "DEV",
                    "--mentioned-or-assignee",
                    "me",
                    other,
                    "me",
                ])
                .is_err(),
                "{other} should conflict with --mentioned-or-assignee"
            );
        }
        assert!(Cli::try_parse_from([
            "taskshoot",
            "tasks",
            "--project",
            "DEV",
            "--mentioned-or-assignee",
            "me",
        ])
        .is_ok());
    }

    fn tasks_include_archived_of(args: &[&str]) -> bool {
        match Cli::try_parse_from(args).expect("should parse").command {
            Cmd::Tasks {
                include_archived_projects,
                ..
            } => include_archived_projects,
            _ => panic!("expected the tasks command"),
        }
    }

    #[test]
    fn tasks_include_archived_projects_only_applies_to_a_sweep() {
        assert!(!tasks_include_archived_of(&["taskshoot", "tasks"]));
        assert!(tasks_include_archived_of(&[
            "taskshoot",
            "tasks",
            "--include-archived-projects",
        ]));
        // an explicit --project always lists that project, archived or not, so
        // the flag would have nothing to widen -- reject it instead of ignoring it
        assert!(Cli::try_parse_from([
            "taskshoot",
            "tasks",
            "--project",
            "OLD",
            "--include-archived-projects",
        ])
        .is_err());
    }

    fn tasks_count_flag_of(args: &[&str]) -> bool {
        match Cli::try_parse_from(args).expect("should parse").command {
            Cmd::Tasks { count, .. } => count,
            _ => panic!("expected the tasks command"),
        }
    }

    #[test]
    fn tasks_count_defaults_to_off_and_combines_with_the_filters() {
        assert!(!tasks_count_flag_of(&["taskshoot", "tasks"]));
        // the form an agent uses to decide whether there is any work at all
        assert!(tasks_count_flag_of(&[
            "taskshoot",
            "tasks",
            "--bot-ready",
            "true",
            "--status",
            "起案",
            "--mentioned-or-assignee",
            "me",
            "--count",
        ]));
        // --count reports on the same list, so it is not exclusive with --json
        // (that pair is what makes the output machine readable)
        assert!(tasks_count_flag_of(&[
            "taskshoot",
            "tasks",
            "--count",
            "--json"
        ]));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
