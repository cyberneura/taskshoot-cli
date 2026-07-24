# taskshoot

Command-line client for [Taskshoot](https://taskshoot.com) — task management with a
Slack-like chat UI.

`taskshoot` lets you drive the task operations you would otherwise do in the web UI
(list, search, claim, comment, complete, …) from your terminal. It is designed to be
friendly to **both humans and AI agents**: every command supports `--json` output and
returns exit code `0` on success / `1` on error, so it composes cleanly in scripts and
autonomous agent loops.

> Taskshoot itself is closed-source; this CLI is an open-source API client and contains
> no server-side logic.

## Installation

### Homebrew (macOS / Linux)

```bash
brew install cyberneura/tap/taskshoot
```

### Shell script (macOS / Linux)

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/cyberneura/taskshoot-cli/releases/latest/download/taskshoot-installer.sh | sh
```

### PowerShell (Windows)

```powershell
powershell -c "irm https://github.com/cyberneura/taskshoot-cli/releases/latest/download/taskshoot-installer.ps1 | iex"
```

### Cargo

```bash
cargo install taskshoot
```

### From source

```bash
git clone https://github.com/cyberneura/taskshoot-cli
cd taskshoot-cli
cargo install --path .   # installs to ~/.cargo/bin/taskshoot
```

Prebuilt binaries for macOS (Apple Silicon / Intel), Linux (x86_64 / aarch64) and
Windows (x86_64) are attached to every [GitHub release](https://github.com/cyberneura/taskshoot-cli/releases).

## Authentication

An API key (`tssk-...`) is issued from Taskshoot under `/settings/api-keys` (a personal
key), or from a Bot user in organization management. Write operations require a
write-scoped key. Organization-scoped keys (deprecated) are not accepted.

The key and organization are resolved in this order of precedence:

1. **Environment variables** — `TASKSHOOT_API_KEY` / `TASKSHOOT_CLI_ORGANIZATION`.
   Use this when CI or an AI agent passes credentials directly, or in any process where
   1Password Touch ID approval is unavailable.
2. **Getter command** — the command in `TASKSHOOT_CLI_ENV_GETTER_COMMAND` is executed
   (without a shell) and its stdout is parsed as an env-file (`KEY=VALUE`, `#` comments
   allowed).
3. **`.loadenv.sh` discovery** — `taskshoot` searches upward from the current directory,
   then upward from the executable's directory, for a `.loadenv.sh`, and extracts only
   the `export TASKSHOOT_CLI_ENV_GETTER_COMMAND=...` line to run as in (2) (it never
   executes the whole file as a shell script). **A discovered file is only trusted after
   you explicitly approve it with `taskshoot trust <path>`** (direnv-style; the approval
   is recorded with a content hash in `~/.config/taskshoot/trusted-loadenv`, and editing
   the file requires re-approval). This prevents an untrusted repository from running
   arbitrary commands when you invoke the CLI inside it.

> **`taskshoot trust` is only needed for path 3.** With the setups below (including
> exports via a shell profile or wrapper script), `.loadenv.sh` discovery never runs, so
> neither `.loadenv.sh` nor `trust` is required. Running `taskshoot trust` with no
> argument in such a setup prints "trust is not needed" and exits successfully.
>
> - `TASKSHOOT_CLI_ENV_GETTER_COMMAND` is exported (discovery is always skipped).
> - Both `TASKSHOOT_API_KEY` **and** `TASKSHOOT_CLI_ORGANIZATION` are exported.
>
> Note that `TASKSHOOT_API_KEY` **alone is not enough**: org-scoped commands
> (`projects` / `tasks` / …) that receive neither `--org` nor `TASKSHOOT_CLI_ORGANIZATION`
> will **search for a `.loadenv.sh` in order to resolve the organization**. In that case
> an untrusted candidate is skipped and the command fails with
> `TASKSHOOT_CLI_ORGANIZATION is not set`, so trust (or an explicit org) is required.
> `me` / `orgs` (which need no org) work with the key alone.

### 1Password setup example

Create an item in your 1Password vault with an env-file-formatted field:

```
TASKSHOOT_CLI_ORGANIZATION=cyberneura
TASKSHOOT_API_KEY=tssk-...
```

Put a `.loadenv.sh` in a **directory that will be searched** and register trust.
Discovery only looks at ancestors of the current directory and ancestors of the
executable — **it never looks into child directories**:

- **Using it inside a repository**: place it at the repository root (found from anywhere
  inside the repo).
- **Using a `cargo install`ed binary from anywhere**: place it at `~/.loadenv.sh`
  (`$HOME` is an ancestor of `~/.cargo/bin`, so it is found via the executable-side
  search), or export `TASKSHOOT_CLI_ENV_GETTER_COMMAND` from your shell profile
  (no discovery needed).

```sh
echo 'export TASKSHOOT_CLI_ENV_GETTER_COMMAND='"'"'op read "op://development/taskshoot/taskshoot-cli"'"'"' > .loadenv.sh
taskshoot trust .loadenv.sh   # once; re-run if you edit the file
```

### API endpoint

The default is `https://taskshoot-api.cyberneura.com`. To point at a local dev server:

```bash
export TASKSHOOT_API_ORIGIN=http://127.0.0.1:8008
```

## Usage

Every command supports `--json` (raw JSON output) and `--org <code>` (override the
organization).

```bash
taskshoot me                                   # who am I (verify auth)
taskshoot orgs                                 # organizations you can access (works with no org set)
taskshoot projects                             # list projects
taskshoot workflows --project DEV              # progress flows and stages (value / label / terminal)
taskshoot categories --project DEV             # task categories (id / name)

taskshoot tasks --project DEV                  # list tasks
taskshoot tasks --project DEV --status draft --assignee me
taskshoot tasks --project DEV --status draft,in-progress    # multiple values are OR'd
taskshoot tasks --project DEV --status draft --status 40    # repeating the flag also ORs
taskshoot tasks --project DEV --exclude-status done         # exclude a status (mutually exclusive with --status)
taskshoot tasks --project DEV --exclude-phase done,invalid,rejected,cancelled  # exclude terminal tasks by phase
taskshoot tasks --project DEV --mentioned me   # tasks that @-mention you
taskshoot tasks --project DEV --mentioned suzuki   # a specific person (handle / display name / id)
taskshoot tasks --project DEV --untracked      # casual tasks only
taskshoot tasks --project DEV --bot-ready true # only tasks a bot may pick up

taskshoot search "search index"                # org-wide task search (--limit 1-50)
taskshoot search DEV-12                         # a KEY-number reference matches directly

taskshoot task show DEV-12
taskshoot task create --project DEV --title "New feature" --description "..." --assignee me
taskshoot task create --project DEV --content "a casual note"   # untracked (no number)
taskshoot task update DEV-12 --status in-progress --progress 50
taskshoot task update DEV-12 --bot-ready true  # bot-ready flag (change is logged)
taskshoot task update DEV-12 --category Dev    # set category (name or id; "" clears it)
taskshoot task claim DEV-12                     # assignee=me + move to in-progress (--status overrides)
taskshoot task claim DEV-12 --if-unassigned     # claim only if unassigned (409 if already taken)
taskshoot task complete DEV-12 --comment "Done"     # move to the terminal stage
taskshoot task comment DEV-12 "progress update" --file ./screenshot.png
taskshoot task events DEV-12                     # show the thread
taskshoot task track <uuid> --project DEV        # untracked -> tracked (assigns a number)
taskshoot task cancel DEV-12 --reason "duplicate"
taskshoot task resume DEV-12
```

Notifications (mention inbox):

```bash
taskshoot notifications list                   # your notifications (newest first) + unread count
taskshoot notifications list --unread-only     # unread only
taskshoot notifications list --limit 50 --json # for AI agents (max 100)
taskshoot notifications read <id> [<id> ...]   # mark ids read (needs a write key)
taskshoot notifications read --all             # mark all read
```

## Command notes

- Task references are `KEY-number` (e.g. `DEV-12`). Untracked tasks have no number, so
  reference them by UUID + `--project`.
- `--status` accepts a label (e.g. `in-progress`) or a numeric value (e.g. `40`). For
  single-task operations (`task update`, …) it is resolved against that task's workflow.
  For `tasks --status` (the list filter) it is resolved against all of the project's
  workflows, and if one label maps to more than one value it errors (use a numeric value).
- `tasks --status` / `tasks --exclude-status` **accept multiple values** (comma-separated
  or by repeating the flag). Multiple values are OR'd: `--status` means "matches any",
  `--exclude-status` means "matches none". The two are mutually exclusive. Both are
  server-side filters, so they apply *before* the `limit` truncation (more accurate than
  filtering with `jq`).
- **Caveat when passing a label to `--exclude-status`**: the server can only filter status
  by its numeric value, so if another workflow assigns a different label to the same value,
  tasks with that label are excluded too. The `--status` (include) path can correct this
  client-side by re-filtering the response by label text, but exclude cannot (the rows are
  already gone from the response). When this is detected a warning is printed to stderr;
  to exclude precisely, look up the per-workflow numeric value with
  `taskshoot workflows --project <KEY>` and pass the number.
- **`status` (stage) and `phase` (lifecycle) are independent axes.** `status` is the
  stage (draft → … → done); `--status` / `--exclude-status` filter on it. `done`,
  `invalid`, `rejected`, `cancelled` are **phases**, not statuses. In particular
  "invalid" only sets `phase=invalid` and leaves the status unchanged (a task can be
  invalid while still in the "draft" stage), so `--exclude-status` cannot reliably remove
  terminal tasks. **To drop terminal tasks (done / invalid / …), use `--exclude-phase`**
  (e.g. `--exclude-phase done,invalid,rejected,cancelled`). Values may be a label
  (Japanese) or the english value
  (`done` / `invalid` / `rejected` / `cancelled` / `in_progress` / `acceptance` /
  `pre_approval`). The JSON `phase` field is returned as the english value.
- `task complete` moves to the workflow's terminal stage. If the project's workflow has an
  acceptance flow, the task enters the acceptance phase rather than being completed (by
  design). `--comment` is posted to the thread after completion succeeds.
- `tasks --mentioned <user>` narrows to "tasks whose description or a comment @-mentions
  that user" (server-side filter `mentioned_user_id`). The user is given as `me` / handle
  name / display name / user id, same as `--assignee`. Matching follows the same rules as
  the web UI's mention rendering (handle_name, or a default handle derived from the email
  local part), and includes mentions of MentionGroups the user belongs to (`@dev-team`, …).
- `--category` (create / update) takes a category name (case-insensitive) or id. List them
  with `taskshoot categories --project <KEY>`. `update --category ""` clears it.
- `me` / `orgs` / `notifications` work with no organization set
  (`TASKSHOOT_CLI_ORGANIZATION` unset); notifications are user-scoped and cross-org.
- `search` searches across all projects in the organization (`/task-search/` API). The
  server side is a hybrid of bigram (substring) + vector (semantic) search over title,
  description and comment bodies; a `KEY-number` or bare number matches directly.

### Example AI-agent flow

```bash
taskshoot tasks --project DEV --bot-ready true --status draft --json  # find pickable, unstarted tasks
taskshoot task claim DEV-12 --if-unassigned --json     # claim it (avoids double-processing: 409 if taken)
taskshoot task comment DEV-12 "Starting now" --json
# ... development ...
taskshoot task complete DEV-12 --comment "Done. PR: <URL>" --json
```

## Development

```bash
cargo test          # unit tests (task_ref parsing / env parsing / stage resolution)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
