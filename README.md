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

This CLI is designed with a strong bias toward keeping the API key in a cloud secret
store — 1Password, AWS Systems Manager Parameter Store, or anything else with a command
line client. `config_override_command` (below) fetches the key at the moment it is
needed, so the key never has to sit in plaintext on disk, and rotating it in the store is
enough to rotate it everywhere.

The key and organization are resolved in this order of precedence:

1. **Command line flags** — `--org` overrides the organization for a single invocation.
2. **Environment variables** — `TASKSHOOT_CLI_API_KEY` / `TASKSHOOT_CLI_ORGANIZATION` /
   `TASKSHOOT_CLI_API_ORIGIN`. Use this when CI or an AI agent passes credentials directly,
   or in any process where 1Password Touch ID approval is unavailable.
3. **Config file** — `~/.config/taskshoot/config.yml`, optionally overlaid with the YAML
   printed by its `config_override_command`.

### Config file

```bash
taskshoot config init   # create ~/.config/taskshoot/config.yml (mode 600)
taskshoot config path   # print the file that will be read
taskshoot config show   # print the merged result (the API key is masked)
```

`config.yml` is preferred; `config.yaml` is used when `config.yml` does not exist. Only
one of the two is ever read. The file may hold a plaintext API key, so it is created
with mode `600`, and permissions are tightened back to `600` whenever it is read.

```yaml
api_key: tssk-...
organization: cyberneura
api_origin: https://taskshoot-api.cyberneura.com   # optional
```

### Keeping the key out of the file

`config_override_command` runs a command **without a shell** (so pipes, redirects and
substitutions are not available) and merges the YAML it prints on stdout over the rest of
the file. Mappings are merged recursively; scalars and sequences are replaced wholesale.
Values from the command win over values written in the file.

```yaml
# ~/.config/taskshoot/config.yml
api_origin: http://127.0.0.1:8008
config_override_command: op read "op://development/taskshoot/config-yaml"
```

Whatever the command prints must be a YAML mapping, so the stored secret looks like this:

```yaml
api_key: tssk-...
organization: cyberneura
```

Any secret store with a command line client works the same way. AWS Systems Manager
Parameter Store, for example:

```yaml
config_override_command: aws ssm get-parameter --name /taskshoot/config --with-decryption --query Parameter.Value --output text
```

Notes:

- The command runs on **every invocation**, so a slow or interactive helper (`op read`
  with Touch ID) is felt on every command. Wrap it in your own caching script if that
  matters.
- A non-zero exit, empty output, or output that is not a YAML mapping is an error. The
  CLI never silently falls back to a different credential.
- `config_override_command` in the fetched YAML is ignored — there is no second round.
- The command does not inherit `TASKSHOOT_CLI_API_KEY` from the environment, so it cannot
  read back a key that is already set.
- The config file is skipped entirely when `TASKSHOOT_CLI_API_KEY`, `TASKSHOOT_CLI_API_ORIGIN`
  and the organization all come from flags or the environment, since it could not
  contribute anything. Set all three to keep the command from running in a bot loop.

### Migrating from 0.1.0

Two things changed in 0.2.0.

**Environment variables were renamed** so that every variable this CLI reads shares the
`TASKSHOOT_CLI_` prefix:

| 0.1.0 | 0.2.0 |
|---|---|
| `TASKSHOOT_API_KEY` | `TASKSHOOT_CLI_API_KEY` |
| `TASKSHOOT_API_ORIGIN` | `TASKSHOOT_CLI_API_ORIGIN` |
| `TASKSHOOT_CLI_ORGANIZATION` | unchanged |

The old names are not read. The CLI prints a notice on stderr when it sees one still set,
so a stale variable cannot quietly look like it is in effect.

**`.loadenv.sh` discovery, `TASKSHOOT_CLI_ENV_GETTER_COMMAND` and the `taskshoot trust`
subcommand were removed.** Replace them with the config file:

1. Run `taskshoot config init`.
2. Move the getter command to `config_override_command:` in that file.
3. Change what the command prints from `KEY=VALUE` lines to YAML — `TASKSHOOT_API_KEY`
   becomes `api_key`, `TASKSHOOT_CLI_ORGANIZATION` becomes `organization`, and
   `TASKSHOOT_API_ORIGIN` becomes `api_origin`.
4. Delete `~/.config/taskshoot/trusted-loadenv` and any leftover `.loadenv.sh`.

Nothing outside `$HOME` is read any more, not even to detect a leftover `.loadenv.sh`, so
running the CLI inside an untrusted checkout cannot make it open a file there. A setup
that still exports `TASKSHOOT_CLI_ENV_GETTER_COMMAND` is reported on stderr, and one that
supplies nothing usable fails with the migration steps above rather than falling back to
a key or an API origin you did not intend.

### API endpoint

The default is `https://taskshoot-api.cyberneura.com`. To point at a local dev server,
either export the variable or set `api_origin:` in the config file:

```bash
export TASKSHOOT_CLI_API_ORIGIN=http://127.0.0.1:8008
```

## Usage

Every command supports `--json` (raw JSON output) and `--org <code>` (override the
organization).

```bash
taskshoot me                                   # who am I (verify auth)
taskshoot orgs                                 # organizations you can access (works with no org set)
taskshoot projects                             # list projects
taskshoot workflows --project DEV              # progress flows and stages (value / label / terminal)
taskshoot categories --project DEV             # task categories (id / name / color / ordering / state)

taskshoot category create --project DEV --name Bug --color red --ordering 5
taskshoot category update Bug --project DEV --name Defect       # rename (name or id)
taskshoot category update Defect --project DEV --active false   # hide from the task form

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
- `--category` (task create / update) takes a category name (case-insensitive) or id. List
  them with `taskshoot categories --project <KEY>`. `task update --category ""` clears it.
- `category create` / `category update` manage the categories themselves and **require the
  manager role or higher** (member returns `403 manager role required`). `category update`
  takes a name (case-insensitive) or id as its positional argument, and needs at least one
  of `--name` / `--color` / `--ordering` / `--active`. `--name` is truncated to 100 and
  `--color` to 20 characters by the server, and `--ordering` must be between 0 and
  2147483647.
- The CLI intentionally has no `category delete`. The API does support deleting, but that
  detaches the category from the tasks already using it, so hide it with
  `category update <name> --active false` instead: it disappears from the task form while
  staying on those tasks.
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
