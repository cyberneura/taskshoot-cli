---
name: taskshoot-agent-loop
description: Autonomously find, claim, implement, and complete Taskshoot tasks in a loop. Use when asked to process taskshoot tasks automatically or run an unattended agent loop. Includes an atomic claim mechanism so multiple agents never double-process a task.
---

# Taskshoot agent loop

An autonomous loop that picks up `bot_ready` Taskshoot tasks, claims them atomically,
implements them, and completes them — one task per iteration, repeating until no
candidates remain.

Read the `taskshoot-cli` skill first for command syntax, authentication, and the
`status` vs `phase` distinction. This skill covers only the loop logic and the mechanism
that stops multiple agents from double-processing a task.

## Prerequisites

- The `taskshoot` CLI is installed and authenticated with a **write-scoped API key.**
- Give each agent its **own distinct key / bot user.** The identity behind the key is the
  loop's "self ID" and is what makes double-processing detectable.
- Candidate tasks have been marked **`bot_ready=true`** (in the web UI or via the API).
  This is the human approval gate: only tasks a person has said a bot may take flow into
  the loop.

## The two flags that gate the loop

- **`bot_ready`** — the human permission gate. A person sets `bot_ready=true` to say "a
  bot may pick this up". **Never touch a task with `bot_ready=false`.** Changes to the
  flag are recorded in the task's history.
- **`--mentioned-or-assignee me`** — scopes the loop to work actually addressed to this
  agent. Work is handed to a bot in two ways: by @-mentioning it (including groups it
  belongs to) or by putting it in the assignee field. This flag is the union of the two,
  so each agent picks up everything directed at it and nothing directed at another.

## How double-processing is prevented (atomic claim)

Seeing a task in a list is not enough to start it — two agents can list the same task at
the same time. The exclusion is done by an **atomic claim**, not by list-then-check:

- `taskshoot task claim <ref> --if-unassigned` takes a **row lock on the server** and
  assigns the task to you **only if it is currently unassigned (or already yours).** If
  someone else — a human or another bot — already holds it, the command fails with **HTTP
  409** (exit code 1).
- This is a compare-and-swap: if two agents claim simultaneously, **exactly one
  succeeds.** There is no read-back race window.
- So the loop's exclusion rule is simply: **claim succeeded -> the task is yours; 409 ->
  move on to the next candidate.**

> Plain `claim` without `--if-unassigned` is last-writer-wins and forcibly reassigns the
> task to you (for a human taking a task over). In a bot loop, **always pass
> `--if-unassigned`.**

## Loop procedure

One completed task per iteration.

### 0. Establish self ID (once, at loop start)

```bash
ME=$(taskshoot me --json | jq -r .id)
```

### 1. Find candidates

Filter to tasks that are bot-ready, addressed to this agent, in an initial stage, and not
terminal. Do the filtering **server-side**, not with `jq` — `jq` can only filter within
the page the API already returned, so it silently drops older matches.

```bash
taskshoot tasks --project DEV \
  --bot-ready true \
  --mentioned-or-assignee me \
  --status draft \
  --exclude-phase done,invalid,rejected,cancelled \
  --json | jq --arg me "$ME" '[.[] | select(.assignee == null or .assignee.id == $me)]'
```

- Use `--status <initial-stage>` for the project's not-yet-started stage(s); pass several
  OR'd values (comma-separated) if there is more than one.
- **Always pass `--exclude-phase done,invalid,rejected,cancelled`.** These are *phases*,
  not statuses. In particular an "invalid" task keeps its original stage (e.g. "draft"),
  so it would otherwise slip into a `--status draft` candidate list and `--exclude-status`
  could not remove it.
- Filtering to `assignee == null or assignee.id == $ME` locally just trims obviously-taken
  tasks to reduce wasted 409s; the real exclusion is the atomic claim in step 3. Keeping
  your own id matters with `--mentioned-or-assignee`: the assignee half returns tasks
  already assigned to you, and `claim --if-unassigned` accepts those. Dropping tasks
  assigned to *someone else* is the point — a mention does not override the fact that
  another person or bot is already on it (`$ME` is the id resolved in step 0).

If no candidates remain, stop (see "Termination").

### 2. Pick one

Choose by priority (smaller = higher) and break ties randomly, so that concurrent agents
tend to pick different tasks and collide less. Build the `KEY-number` reference from the
result:

```bash
SEL=$(echo "$CANDIDATES" | jq -c 'sort_by(.priority) | .[0]')
PROJECT=$(echo "$SEL" | jq -r '.project_key')
REF=$(echo "$SEL" | jq -r 'if .number then "\(.project_key)-\(.number)" else .id end')
```

> **UUID pitfall:** the `.id` in list JSON is a UUID. Passing a bare UUID to
> `task claim` / `task show` fails with "--project is required when referencing a task by
> UUID". Build the `KEY-N` slug as above (works without `--project`), or, for untracked
> tasks that have no `.number`, add `--project "$PROJECT"` to every `task` subcommand.
> When piping into `jq`, do not merge stderr (`2>&1`) — errors are non-JSON text on
> stderr and would break the parse.

### 3. Claim atomically

```bash
if taskshoot task claim "$REF" --if-unassigned --json; then
  echo "claimed $REF"
else
  echo "lost race on $REF (already claimed), skipping"
  # -> go back to step 1 and pick another candidate
fi
```

A 409 (non-zero exit) means another agent got there first — do not touch it, move on.
On success the task is yours. Optionally re-read the assignee to confirm it is still you
(catches a human reassigning it mid-flight):

```bash
OWNER=$(taskshoot task show "$REF" --json | jq -r '.assignee.id // ""')
[ "$OWNER" = "$ME" ] || echo "not mine anymore, skipping"
```

Then, if you want start time recorded, set it and leave a starting comment (`claim` does
not set `started_at` automatically):

```bash
taskshoot task update "$REF" --started-at "$(date -Iseconds)" --json
taskshoot task comment "$REF" "Bot is starting on this task." --json
```

### 4. Implement

```bash
taskshoot task show "$REF"
taskshoot task events "$REF"
```

- Read the task body and thread, then do the work in the relevant repository (implement,
  test, review).
- Leave progress at checkpoints: `taskshoot task update "$REF" --progress 50`, plus
  comments as you go.
- **If you get stuck** (missing information, a decision you should not make on your own, a
  destructive or irreversible action), do not force a completion. Leave a comment
  describing what you need and move on, so a human can pick it up:
  `taskshoot task comment "$REF" "Need clarification: ..."`.

### 5. Complete

When the work is verified, record completion time and 100% progress, then complete:

```bash
taskshoot task update "$REF" --completed-at "$(date -Iseconds)" --progress 100 --json
taskshoot task complete "$REF" --comment "Done. PR: <URL> / Summary: ..." --json
```

`complete` does not set `completed_at` automatically, so set it explicitly just before.
If the project has an acceptance flow, `complete` moves the task into the acceptance phase
rather than fully completing it — that is by design.

### 6. Next iteration

Go back to step 1 and repeat until candidates are exhausted.

## Skipping a task you decide not to do

If you inspect a claimed or candidate task and conclude the bot should not do it (out of
scope, needs a human, ambiguous), mark it so the loop stops re-picking it:

```bash
taskshoot task update "$REF" --bot-ready false --comment "Skipping: <reason>" --json
```

Setting `bot_ready=false` removes it from every future candidate query (step 1 filters on
`--bot-ready true`) and the change is logged, so a human can see why and re-enable it if
appropriate.

## Termination

- Stop when the candidate set (bot-ready, unassigned, initial-stage, non-terminal) is
  empty.
- If you lose the claim race repeatedly, or keep failing on the same task, stop to avoid a
  tight spin loop and report the situation instead of retrying forever.
- To keep running unattended, drive this skill from a scheduler (a recurring interval, a
  cron job, or a long-running process). Space out the CLI calls and wait when a pass finds
  nothing.

## Safety principles

- **Never touch a task with `bot_ready=false`.** It is the human permission gate.
- **Always pass `--if-unassigned` to `claim`.** It is the atomic CAS that prevents
  double-processing. On a 409, give up on that task and move on.
- **Do not auto-complete destructive or irreversible work** (deployments, production
  migrations, sending external messages). Implement it if appropriate, but leave the
  completion to a human — post a comment explaining the pending action instead.
- **Never expose a raw API key** in stdout or a transcript; pass it via env vars or a
  getter command.
- **Re-check the assignee at each checkpoint** (before starting, before completing) so you
  notice if a human reassigned the task while you were working.
