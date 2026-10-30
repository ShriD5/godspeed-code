# Explicit agent installation

Settings → Providers → **Install** runs on the selected device, as its user. Merely
listing or detecting agents never invokes these CLI installers. The table below
was checked with `curl -fsSL` against the linked vendor documentation on
2026-09-21. Commands download the vendor's current release, except for the existing
pinned Antigravity archive.

| Agent / documentation | Unix command (Linux and macOS) | Native Windows command |
| --- | --- | --- |
| [Claude Code](https://code.claude.com/docs/en/setup) | `curl -fsSL https://claude.ai/install.sh \| bash` | `irm https://claude.ai/install.ps1 \| iex` |
| [Codex](https://github.com/openai/codex/blob/main/README.md) | `curl -fsSL https://chatgpt.com/codex/install.sh \| sh` | `irm https://chatgpt.com/codex/install.ps1 \| iex` |
| [Cursor](https://cursor.com/docs/cli/installation) | `curl https://cursor.com/install -fsS \| bash` | `irm 'https://cursor.com/install?win32=true' \| iex` |
| [OpenCode](https://opencode.ai/docs/) | `curl -fsSL https://opencode.ai/install \| bash` | `npm.cmd install -g @opencode/cli` |
| [Pi](https://pi.dev/docs/latest) | `curl -fsSL https://pi.dev/install.sh \| sh` | `npm.cmd install -g --ignore-scripts @earendil-works/pi-coding-agent` |
| [Grok](https://docs.x.ai/developers/release-notes) | `curl -fsSL https://x.ai/cli/install.sh \| bash` | `npm.cmd install -g @xai-official/grok` |
| [Hermes](https://hermes-agent.nousresearch.com/docs/getting-started/installation) | `curl -fsSL https://hermes-agent.nousresearch.com/install.sh \| bash` | `irm https://hermes-agent.nousresearch.com/install.ps1 \| iex` |
| [Devin](https://cli.devin.ai/) | macOS, when brew resolves: `brew install --cask devin-cli`; otherwise `curl -fsSL https://cli.devin.ai/install.sh \| bash` | `irm https://static.devin.ai/cli/setup.ps1 \| iex` |
| [fx](https://fx.sh/docs) | `curl -fsSL https://fx.sh/setup.sh \| bash` | Manual setup (no native Windows installer) |
| Antigravity | Existing verified 1.1.1 archive in `acp::antigravity_archive`; no shell command | Same archive path on supported architectures |

Unix shell commands run through `sh -c`, with PATH augmented from the user's login
shell and npm toolchain. Native Windows `irm` commands run through
`powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`.
Windows npm commands use the resolved npm launcher (normally `npm.cmd`).

If shell prerequisites are absent, these documented npm alternatives are used
when npm resolves: `npm install -g @openai/codex`,
`npm install -g @opencode/cli`,
`npm install -g --ignore-scripts @earendil-works/pi-coding-agent`, and
`npm install -g @xai-official/grok`. Codex also falls back to npm on Windows when
PowerShell is unavailable. Grok's npm route is documented in its
[enterprise guide](https://docs.x.ai/build/enterprise); its
[published package metadata](https://registry.npmjs.org/@xai-official/grok/latest)
includes Windows binaries. No action is offered when none of the documented
methods' prerequisites resolves, or for Mock. The row then keeps a manual hint;
shell hints on Windows can be used in WSL.

Install processes receive `CI=1`, `NONINTERACTIVE=1`, `TERM=dumb`, no stdin/TTY,
and no inherited `GODSPEED_*` or nested Claude environment markers. Output is bounded
and credentials are redacted before errors reach Settings. Cancel, timeout
(15 minutes), or dropping the request kills the owned process tree. There is no
automatic elevation: an installer that needs sudo or an interactive prompt may
fail. Cancelling does not undo files an installer already wrote.

After installation, executable version caches for the agent are invalidated and
the normal installed probe runs again. A missing binary produces PATH guidance;
the running app's login-shell snapshot is not refreshed. Authentication remains a
separate step. Hermes may additionally require its documented ACP Python extra
for agent sessions; the Install action checks CLI detection only.

## Verification

The `installer-fixture` feature exposes an explicit command injection seam for
integration tests. The engine's command environment override is compiled only
for unit tests, never production. Tests exercise successful detection, failure,
missing binaries, cancellation, duplicate requests, process descendants, bounded
redaction, platform prerequisites, and version-cache invalidation.

The opt-in engine test `rpc::tests::installer_rpc_npm` runs the documented Codex
npm alternative in a disposable prefix and isolated home, through `InstallHarness`.
It asserts that the returned Settings descriptor changes from uninstalled/disabled
to installed/enabled. It never runs a vendor shell installer. Set `TMPDIR` to a
scratch directory before running it with `--ignored --nocapture`.

The iOS app adds no Install action. `HarnessDescriptor.can_install` already has
`#[serde(default)]` (`crates/engine/src/registry.rs`); iOS's `WireHarness: Decodable`
in `apps/ios/Godspeed/Sync/WorkspaceStore.swift` ignores unrecognized keys, including
`canInstall`. `HarnessCatalog` continues consuming the existing mapped fields.

# More agents from the ACP registry

Settings → Providers → **More agents** lists the public ACP agent registry
(`https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`, the index
T3 Code reads) as the selected device sees it: name, publisher, version, install
size and route. One click installs, updates or uninstalls an agent; an installed
agent then appears as an ordinary provider (`HarnessId::Acp`, wire name
`acp:<registry id>`) driven by the generic ACP driver (`AcpHarness::registry_agent`):
turn-boundary steering, models and modes discovered from its own session.

Registry content is untrusted data (`acp_registry.rs`):

- The index is capped at 1 MiB and 512 agents and cached under
  `{data_dir}/acp-registry/` (refreshed after five minutes; installs always refetch).
  Each agent is validated field by field as T3 does (bounded ids, names, versions,
  arguments; HTTPS URLs without credentials; exact-version packages); an invalid agent
  is dropped. Godspeed also refuses `cmd` paths that leave the archive and registry env
  that sets `PATH`, `HOME`, `NODE_OPTIONS`, `LD_*`, `DYLD_*` or `GODSPEED_*`.
- **Nothing runs unless it is pinned to a checksum.** Binary builds install only when
  the registry pins their SHA-256 (T3 accepts unpinned binaries; Godspeed does not);
  the download is hashed while it streams and a mismatch discards it. Archives are
  `.zip`, `.tar.gz` or `.tar.bz2`, or a bare executable; links, devices and paths
  outside the install dir are refused. npm packages install at the registry's exact
  version with `--ignore-scripts` (no lifecycle script runs), into their own prefix,
  and the installed tarball's lockfile `integrity` must equal the sha512 npm publishes
  for that version. uvx (Python) agents are listed but not installable yet.
- Installs land in `<adapters>/acp-registry/<agent>/<version>/` (the managed adapters
  dir above), built in a `.tmp-*` sibling and renamed into place after the entry
  resolves and the receipt `godspeed-agent.json` is written. An update installs the
  new version first, then removes the old one; uninstall removes the agent's folder.
- Registry entries Godspeed already drives natively (Claude, Codex, Cursor, Devin,
  Grok, Antigravity, OpenCode, Pi) are not offered a second time.

Routes for the agents Godspeed promises, as the live registry listed them on 2026-10-28
(the live install + `initialize` check is `tests/acp_registry_live.rs`; its last result
is recorded below the table):

| Agent | Route | Notes |
| --- | --- | --- |
| Factory Droid (`factory-droid`) | npm `droid`, args `exec --output-format acp-daemon` | `--ignore-scripts` keeps its postinstall (an unpinned binary relink) from running; its JS shim launches the platform package |
| GitHub Copilot CLI (`github-copilot-cli`) | npm `@github/copilot`, `--acp` | platform binary arrives as an integrity-checked optional dependency |
| Gemini CLI (`gemini`) | npm `@google/gemini-cli`, `--acp` | ~99 MB unpacked |
| Kimi CLI (`kimi`) | SHA-256-pinned `tar.gz`, `acp` | no darwin-x86_64 build in the registry |

Last live run (2026-10-28, macOS arm64, scratch HOME, no sign-in): all four installed with
their checksum verified and answered ACP `initialize` (protocol 1). Advertised sign-in methods:
Droid `device-pairing`, `factory-api-key`; Copilot `copilot-login`; Gemini `oauth-personal`,
`gemini-api-key`, `vertex-ai`, `gateway`; Kimi `login`. Godspeed does not drive those
`authenticate` flows for registry agents yet.

Signing in stays with each agent: set its API key as an instance env var (stored in
the keychain), or sign in once with the agent's own CLI using the same config dir.

# Provider instances

Every provider can have named instances besides its implicit default one
(Settings → Providers → a provider → **Instances**): display name, binary path,
config/home dir, extra env vars, default model, custom model ids and an account
label. Instances live per device in `{data_dir}/provider-instances.json`; env
values that look secret (credential-shaped names such as `*_API_KEY`, `*_TOKEN`,
or token-shaped values) go to the OS keychain (macOS Keychain, Secret Service on
Linux; refused elsewhere) and are never written to settings, RPC replies or logs.

Precedence (`instance.rs`): the instance's binary beats the provider's env override,
which beats PATH detection; the config dir (exported as the provider's own variable:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`, `HERMES_HOME`,
`PI_CODING_AGENT_DIR`, `GEMINI_HOME`, `GEMINI_CLI_HOME`, `COPILOT_HOME`) beats an
extra var of the same name, which beats the chat's worktree env and the engine's
own environment. Cursor, Devin, fx, Muse and OpenCode have no config-dir variable;
use env vars there. OpenCode shares one server across chats, so per-run env reaches
it only through its own config.

A chat records its instance id beside its harness (`ChatConfig.instance`,
`RunRequest.instance`); chats without one run on the default instance exactly as
before. Usage, receipts and telemetry attribute by provider; receipts add the
instance name. Telemetry counts registry agents as `other`.

# fx

fx uses its native `fx acp` server. Install on macOS/Linux with
`curl -fsSL https://fx.sh/setup.sh | bash`, then run `fx login` or `fx setup`
in a terminal to configure the model provider. Godspeed detects PATH and
`~/.local/bin/fx`; `FX_EXECUTABLE` overrides the executable.

Models and reasoning levels come from ACP. The provider selector is excluded
from model discovery. Explicit model selections are sent to fx and errors
propagate instead of silently running a different model. Skills use the existing
host discovery for `.agents/skills`, `.claude/skills`, and `.fx/skills`.

Godspeed writes fx's `ask` policy before every prompt, including resumed sessions:
some fx releases report ask while retaining a saved automatic policy. Godspeed then
decides permissions: Supervised presents requests to the user; Full access
approves them. Streaming, tool calls, cancellation and session loading use the
shared ACP driver. Login and updates remain CLI-managed; native Windows has no
one-click installer.
