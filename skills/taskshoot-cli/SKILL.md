---
name: taskshoot-cli
description: Operate Taskshoot tasks from the command line — list, search, claim, comment, and complete tasks via the `taskshoot` CLI. Use when working with taskshoot.com tasks or when an AI agent needs to pick up and resolve tasks.
---

# Taskshoot CLI

`taskshoot` is a command-line client for [Taskshoot](https://taskshoot.com), a task
manager with a Slack-like chat UI. It drives the task operations you would otherwise do
in the web UI (list, search, claim, comment, complete, ...) from your terminal.

Every command supports `--json` (raw JSON output) and `--org <code>` (override the
organization), and returns exit code `0` on success / `1` on error, so it composes
cleanly in scripts and autonomous agent loops. To run a full autonomous pick-up loop,
also read the `taskshoot-agent-loop` skill.

## Installation

Install the `taskshoot` binary (see the project README for full details):

```bash
brew install cyberneura/tap/taskshoot            # Homebrew (macOS / Linux)
cargo install taskshoot                          # Cargo
# or a shell-script / PowerShell installer, or prebuilt binaries from GitHub Releases.
```

Verify it is on your `PATH` with `taskshoot --help`.

## Authentication

An API key (`tssk-...`) is required. It is issued from Taskshoot under
`/settings/api-keys` (a personal key) or from a Bot user in organization management.
**Write operations require a write-scoped key**; a read key only allows GET-style reads.

The key and organization are resolved in this order:

1. **Command line flags** — `--org` overrides the organization for one invocation.
2. **Environment variables** — `TASKSHOOT_CLI_API_KEY`, `TASKSHOOT_CLI_ORGANIZATION`,
   `TASKSHOOT_CLI_API_ORIGIN`. Use this for CI or an AI agent that passes credentials
   directly.
3. **Config file** — `~/.config/taskshoot/config.yml` (`config.yaml` is the fallback),
   optionally overlaid with the YAML printed by its `config_override_command`.

```bash
taskshoot config init   # create ~/.config/taskshoot/config.yml (mode 600)
taskshoot config path   # print the file that will be read
taskshoot config show   # print the merged result (the API key is masked)
```

```yaml
# ~/.config/taskshoot/config.yml
organization: cyberneura
api_key: tssk-...

# Or fetch the key from a cloud secret store instead of writing it here. The
# command runs without a shell and must print YAML, which is merged over this
# file. This is the intended setup: the key never sits in plaintext on disk.
# config_override_command: op read "op://development/taskshoot/config-yaml"
# config_override_command: aws ssm get-parameter --name /taskshoot/config --with-decryption --query Parameter.Value --output text
```

Notes:

- `config_override_command` runs on every invocation. If it prompts (Touch ID), wrap it
  in your own caching script.
- The config file is skipped entirely when `TASKSHOOT_CLI_API_KEY`, `TASKSHOOT_CLI_API_ORIGIN`
  and the organization all come from flags or the environment. Set all three in a bot
  loop to keep the command from running.
- `me` / `orgs` / `notifications` need no organization and work with the key alone.
- The API endpoint defaults to `https://taskshoot-api.cyberneura.com`. Point at a local
  dev server with `export TASKSHOOT_CLI_API_ORIGIN=http://127.0.0.1:8008` or `api_origin:`
  in the config file.
- **Never print a raw API key to stdout or a transcript.** `taskshoot config show` masks
  it; a getter command's raw output does not.

First, confirm who you are authenticated as:

```bash
taskshoot me      # who am I (id / organization) — use this to verify auth
taskshoot orgs    # organizations you can access (works with no org set)
```

## Core commands

### List and inspect

```bash
taskshoot projects                              # list projects
taskshoot users                                 # organization users (id / handle / display name)
taskshoot user ytyng                            # one user: handle name, display name or user id
taskshoot user me --json                        # your own row (handy to grab your user id)
taskshoot workflows --project DEV               # progress flows and stages (value / label / terminal)
taskshoot categories --project DEV              # task categories (id / name / color / ordering / state)

taskshoot tasks --project DEV                   # list tasks
taskshoot tasks --project DEV,SALES             # several projects merged into one list (OR)
taskshoot tasks                                 # no --project: every non-archived project in the org
taskshoot tasks --include-archived-projects     # ... archived ones too
taskshoot tasks --project DEV --status draft --assignee me
taskshoot tasks --project DEV --status draft,in-progress            # multiple values are OR'd
taskshoot tasks --project DEV --exclude-status done                # exclude a status (exclusive with --status)
taskshoot tasks --project DEV --exclude-phase done,invalid,rejected,cancelled  # drop terminal tasks
taskshoot tasks --project DEV --mentioned me    # tasks that @-mention you
taskshoot tasks --project DEV --mentioned-or-assignee me   # assigned to you OR @-mentioning you
taskshoot tasks --project DEV --bot-ready true  # only tasks a bot may pick up
taskshoot tasks --project DEV --untracked       # casual (numberless) tasks only
taskshoot tasks --bot-ready true --count        # just the number of matches (prints "3")
taskshoot tasks --bot-ready true --count --json # {"count": 3}

taskshoot search "search index"                 # org-wide task search (--limit 1-50)
taskshoot search DEV-12                          # a KEY-number reference matches directly

taskshoot task show DEV-12                       # details (bot_ready / assignee / phase / status)
taskshoot task events DEV-12                     # show the chat thread
```

### Create, update, and lifecycle

```bash
taskshoot task create --project DEV --title "New feature" --description "..." --assignee me
taskshoot task create --project DEV --content "a casual note"       # untracked (no number)

taskshoot task update DEV-12 --status in-progress --progress 50
taskshoot task update DEV-12 --bot-ready true    # bot-ready flag (change is logged)
taskshoot task update DEV-12 --category Dev      # set category (name or id; "" clears it)
taskshoot task update DEV-12 --started-at "$(date -Iseconds)"       # record start time (ISO8601)
taskshoot task update DEV-12 --completed-at "$(date -Iseconds)"     # record completion time (ISO8601)

taskshoot task claim DEV-12                       # assignee=me + move to in-progress
taskshoot task claim DEV-12 --if-unassigned       # claim only if unassigned (409 if already taken)
taskshoot task comment DEV-12 "progress update" --file ./screenshot.png
taskshoot task complete DEV-12 --comment "Done"   # move to the terminal stage
taskshoot task cancel DEV-12 --reason "duplicate"
taskshoot task resume DEV-12
taskshoot task track <uuid> --project DEV          # untracked -> tracked (assigns a number)
```

### Project categories (manager role or higher)

```bash
taskshoot category create --project DEV --name Bug --color red --ordering 5
taskshoot category update Bug --project DEV --name Defect        # rename (name or id)
taskshoot category update Defect --project DEV --active false    # hide from the task form
```

`category create` / `category update` change the project's category list itself (as opposed
to `task update --category`, which only assigns one to a task). They **require the manager
role or higher**; a member gets `403 manager role required`. `category update` needs at
least one of `--name` / `--color` / `--ordering` / `--active`. The server truncates `--name`
to 100 and `--color` to 20 characters, and `--ordering` must be between 0 and 2147483647.

The CLI intentionally has no `category delete`: the API can delete, but that detaches the
category from the tasks already using it. Use `--active false` instead, which hides it from
the task form while keeping it on those tasks.

### Notifications (mention inbox)

```bash
taskshoot notifications list                     # your notifications (newest first) + unread count
taskshoot notifications list --unread-only
taskshoot notifications read <id> [<id> ...]     # mark ids read (needs a write key)
taskshoot notifications read --all
```

### Streaming that inbox (`listen`)

`listen` holds a WebSocket open and prints each notification as one line of JSON, so a bot
can react to a mention in seconds instead of at the next poll.

```bash
taskshoot listen --types task_mentioned          # mentions only, until killed
taskshoot listen --types task_mentioned,task_assigned
taskshoot listen --since <notification id>       # replay from a cursor
taskshoot listen --max-events 1                  # exit after the first one
taskshoot listen --state ./cursor.json           # move the state file
taskshoot listen --no-state                      # do not persist the cursor at all
```

- **stdout** is only notifications (`{"type":"notification_created","notification":{…}}`);
  connection logs go to stderr, so `taskshoot listen | while read -r event` needs no
  filtering. `.notification.task.ref` is the `KEY-N` to pass back to the other commands,
  and `.notification.task.bot_ready` is the human's permission gate.
- **It does not replace polling.** The broadcast has no ACK and no replay, so events
  created while the process is disconnected only return through the reconnect catchup,
  which the server caps at 50. Keep the periodic sweep as the backstop.
- It reconnects on its own (backoff 1s → 60s, jittered) but **exits non-zero when the
  server refused** — a rejected key, or a `--types` / `--since` value it will not take —
  since retrying cannot change that answer.
- Delivery is **at least once**: the cursor is persisted after the event is printed. The
  last 256 delivered ids are stored next to the cursor
  (`~/.config/taskshoot/state/listen-<host>-<user id>.json`) so repeats are dropped, but a
  consumer still has to be idempotent by `notification.id`.

## Key concepts

- **Task references are `KEY-number`** (e.g. `DEV-12`). Untracked tasks have no number, so
  reference them by UUID plus `--project`.
- **`status` (stage) and `phase` (lifecycle) are independent axes.** This is the single
  most common source of mistakes:
  - `status` is the workflow *stage* (draft -> ... -> done). `--status` and
    `--exclude-status` filter on it.
  - `done`, `invalid`, `rejected`, `cancelled` are **phases, not statuses.** In
    particular, marking a task "invalid" only sets `phase=invalid` and leaves the stage
    unchanged (a task can be invalid while still in the "draft" stage). So
    `--exclude-status` cannot reliably remove terminal tasks.
  - **To drop terminal tasks, use `--exclude-phase done,invalid,rejected,cancelled`.**
    Values may be a label or the english value (`done` / `invalid` / `rejected` /
    `cancelled` / `in_progress` / `acceptance` / `pre_approval`). The JSON `phase` field
    is returned as the english value.
- **`tasks --project` accepts multiple projects** (comma-separated or by repeating the
  flag) and merges them into one newest-first list, so a cross-project sweep no longer
  needs a shell loop. **Omitting `--project` entirely lists every non-archived project of
  the organization**, which is what an agent sweeping "my tasks" wants. Archived
  (`inactive`) projects are skipped; pass `--include-archived-projects` to cover them too
  (rejected together with `--project`, which always lists the named project either way).
  One request is sent per project, which is why:
  - `--limit` is **per project** (3 projects can return `3 x limit` tasks).
  - `--status` / `--exclude-status` labels are resolved **per project** (they belong to
    that project's workflows).
  - `--assignee` / `--mentioned` are resolved once (a user id is organization-wide).
  - The table gains a **PROJECT** column in this mode (an untracked task's ref is a bare
    UUID, and `task show <uuid>` needs `--project`) — including a `--project`-less sweep of
    an organization with a single project, since you never typed that key. Output for a
    single explicitly named project is unchanged; `--json` is unaffected either way
    (`project_key` is always in the payload).
- **How a failing project is handled depends on whether you named it:**
  - With `--project`, any failing request fails the whole command
    (`error: project TEST: unknown status ...`), so a partial list is never mistaken for
    a complete one. A project you asked about must not silently match nothing — drop it
    from the list, or pass the status as a numeric value.
  - Without `--project`, a status label a project's workflows do not define is simply
    dropped **for that project**: `--status` values are OR'd, so `--status draft,起案`
    still returns the `起案` tasks of a project that only knows `起案`. A project is
    skipped with a warning on stderr only when *none* of the `--status` labels resolve
    there (the filter would select nothing) or a label is ambiguous within it. Only that
    class of failure is skipped: a transport or API error still fails the command. If
    *every* project is skipped, that is not "no tasks matched", so the command exits `1`
    with the warnings above it — an empty list always means an empty list.
  - The implicit sweep skips archived (inactive) projects unless
    `--include-archived-projects` is given; if *every* project of the organization is
    archived, the command fails and says so rather than reporting an empty list.
- **`--status` / `--exclude-status` accept multiple values** (comma-separated or by
  repeating the flag) and are OR'd; the two flags are mutually exclusive. Both are
  server-side filters, so they apply *before* `--limit` truncation — more accurate than
  filtering the JSON with `jq`. `--status` accepts a label or a numeric value; if a label
  maps to more than one workflow value it errors, so pass the numeric value (look it up
  with `taskshoot workflows --project <KEY>`).
- **`--mentioned <user>`** narrows to tasks whose description or a comment @-mentions that
  user. Give `me`, a handle name, a display name, or a user id. Mentions of groups the
  user belongs to are included.
- **`--mentioned-or-assignee <user>`** is the union of `--assignee` and `--mentioned`
  ("assigned to them **or** @-mentioning them"). Use it for "tasks meant for me" — work is
  handed to an agent either by assigning it or by mentioning it, and each flag alone misses
  half of it. The API filters with AND only, so the CLI sends two requests per project and
  merges them (duplicates folded by task id, re-sorted newest-first), which means `--limit`
  applies to each half. It cannot be combined with `--assignee` or `--mentioned`.
- **`users` / `user <spec>`** list the organization's users and show one of them; use them
  to look up the handle names and ids that `--assignee` / `--mentioned` accept. `spec` is
  the literal `me`, a handle name, a display name (both case-insensitive) or a user id,
  and `user` errors when it matches no one or more than one member. Both read the mention
  candidates, so the handle names are the ones `@` mentions resolve; bots have no handle
  name (`HANDLE` is `-`) and must be referenced by display name or id. Mention groups are
  not listed, and email / role are not available (that endpoint is admin-only).
- **`task complete`** moves to the workflow's terminal stage. If the project has an
  acceptance flow, the task enters the acceptance phase instead of being completed (by
  design). `--comment` is posted to the thread after completion succeeds.
- **`--started-at` / `--completed-at` are not set automatically** by `claim` / `complete`.
  If you want them recorded, set them explicitly with `task update` at the moment of start
  / completion (`date -Iseconds` produces an accepted ISO8601 value; `""` clears to null).
- **`search`** runs an org-wide hybrid (substring + semantic) search over titles,
  descriptions and comment bodies. A `KEY-number` or bare number matches directly.

## Example AI-agent flow

```bash
taskshoot tasks --project DEV --bot-ready true --status draft --json   # find pickable, unstarted tasks
taskshoot task claim DEV-12 --if-unassigned --json     # claim it (avoids double-processing: 409 if taken)
taskshoot task comment DEV-12 "Starting now" --json
# ... development ...
taskshoot task complete DEV-12 --comment "Done. PR: <URL>" --json
```

To run this continuously as an autonomous loop (multiple agents, no double-processing),
read the `taskshoot-agent-loop` skill.
