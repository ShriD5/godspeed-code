# Worktrees that run in parallel

Each chat can run in its own git worktree. A project's **environment** is what lets several of
those worktrees run at the same time: untracked files copied in from the main checkout, a block
of ports per worktree, commands that run when a worktree is created, deleted or woken, and a
branch-name template.

The environment is saved on the device that owns the project, next to its Actions. You edit it
in the session card (Actions → Environment…), or put it in the `env` section of `godspeed.json`
and import it from there. Like Actions, nothing in `godspeed.json` runs until you import it: a
cloned repository can't run commands on your machine by itself.

## `godspeed.json`

```json
{
  "actions": [
    { "name": "Dev", "command": "pnpm dev --port $PORT_WEB", "icon": "play" }
  ],
  "env": {
    "copy": [".env*", { "glob": "node_modules", "symlink": true }],
    "ports": { "count": 3, "names": { "web": 0, "api": 1 } },
    "setup": [
      "createdb app_{{slug}} -T app_dev",
      "echo DATABASE_URL=postgres:///app_{{slug}} >> .env.local"
    ],
    "teardown": "dropdb --if-exists app_{{slug}}",
    "wake": [],
    "branch": "{{prefix}}{{slug}}"
  }
}
```

Every key is optional, and files with only `actions` keep working as before.

| Key | Shape | Meaning |
| --- | --- | --- |
| `copy` | list of globs, or `{ "glob", "symlink" }` objects | Untracked files and folders copied from the main checkout into each new worktree (or symlinked with `"symlink": true`). |
| `ports` | number, or `{ "count", "names" }` | Size of the port block each worktree gets (1–100), with optional names for offsets in it. |
| `setup` | string or list of strings | Commands run in the new worktree, in order, in its "(setup)" terminal. |
| `teardown` | string or list of strings | Commands run in the worktree before it is deleted. |
| `wake` | string or list of strings | Commands run when a sleeping worktree wakes (for example, start the dev server again). |
| `branch` | string | Template for new worktree branch names. Without it, branches are the branch prefix setting plus the generated name. |

### Copying files

- Globs are relative to the project folder; `*` stays within a folder, `**` crosses folders.
- Only untracked files (ignored ones included) are candidates, so tracked files are never
  duplicated. `.git` is never copied, and patterns can't leave the project (`..`, absolute paths).
- An untracked folder such as `node_modules` is matched as a whole. `**` doesn't look inside it;
  name the folder in the pattern (`config/local/*.json`) to reach into it.
- Existing paths in the worktree are never overwritten.
- If copying fails, the new worktree is removed again and the error says why.

### Ports

Each worktree gets its own block of `count` consecutive ports between 41000 and 48999. A block
never overlaps another worktree's block or a port something on the machine already listens on.
The reservation is saved, so a worktree keeps its ports across restarts and sleep, and is
released when the worktree is deleted.

### Variables

Commands can use these templates, filled in when they run:

| Template | Value |
| --- | --- |
| `{{worktree}}` | The worktree folder |
| `{{root}}` | The main checkout |
| `{{project}}` | The project folder's name |
| `{{branch}}` | The worktree's branch |
| `{{slug}}` | The worktree's generated name (`swift-otter`) |
| `{{port}}` | The first port of the block |
| `{{port.web}}`, `{{port.1}}` | A named port, or the port at an offset |

Values are inserted as they are; quote paths that may contain spaces. Saving an environment
checks every template (an unknown name, or a port outside the block, is refused).

The branch template can use `{{prefix}}` (the branch prefix setting), `{{slug}}` (required, so
every worktree gets its own branch) and `{{project}}`. Branches that end in the generated name,
like the default, are still renamed after the chat's title.

Every terminal, Action and agent run in the worktree gets these environment variables:

| Variable | Value |
| --- | --- |
| `GODSPEED_PROJECT_ROOT` | The main checkout |
| `GODSPEED_WORKTREE_PATH` | The worktree folder |
| `GODSPEED_WORKTREE_SLUG` | The generated name |
| `GODSPEED_PORT` | The first port |
| `GODSPEED_PORT_1` … `GODSPEED_PORT_<count-1>` | The other ports of the block |
| `PORT_<NAME>` | Each named port (`PORT_WEB`, `PORT_API`; `-` becomes `_`) |

Agent runs get them for Claude, Codex, Cursor, Pi, Muse and the ACP agents (each starts its own
process per chat). OpenCode shares one server across chats and doesn't get them yet.

### Setup and teardown

- Setup runs after the files are copied, in the worktree's "(setup)" terminal, before the
  project's setup Action (if one is marked "Run automatically on worktree creation"). It stops at
  the first failing step, says which step failed in the terminal, and the session card shows
  "Setup failed at step N". The worktree is kept so you can fix the problem and rerun the step.
- Teardown runs headless before the worktree is removed, each step limited to two minutes. If
  it fails, the worktree is still removed and the error is shown.

## Sleep and wake

"Sleep worktree" in the session card stops every terminal Godspeed started in the worktree,
including Actions and the dev servers they started (their whole process tree), which frees their
ports. The worktree shows as sleeping in the session card and in the branch picker.

Opening a chat in a sleeping worktree, or sending a message to one, wakes it: it is marked awake
and only the `wake` commands run (in a "(wake)" terminal). Nothing else restarts by itself.

## Later

Multi-repo workspaces (one project spanning several repositories, each with its own worktree)
build on this and are not part of it yet.
