# Headless benchmarks: Godspeed vs Zeron

| Headline (median of 6 trials) | Godspeed 0.2.0 | Zeron 0.2.113 |
|---|---:|---:|
| Cold start to ready | 65 ms | 65 ms |
| Idle memory (physical footprint, ready + 30 s) | 12.2 MiB | 10.6 MiB |
| Memory after the workload (400 chats, 201 projects) | 33.9 MiB | 33.2 MiB |
| List 201 projects / 400 chats (p50) | 1.95 ms | 1.74 ms |
| App bundle on disk | 143.8 MiB | 137.0 MiB |

Both are official macOS arm64 release builds, run headless on an M2 MacBook
Air. Zeron was run as shipped. The startup and latency differences are within
trial-to-trial noise. The idle memory difference is not: Godspeed's idle
engine uses about 1.6 MiB (15%) more.

Run on 2026-10-16, 18:56–19:12 IST. 24 fresh-profile trials: 6 each for four
variants, interleaved. No trial failed or was discarded.

This compares the **headless engines only** (`godspeed headless` and
`zeron headless`). Both are driven over the same loopback WebSocket IPC by the
same Python client. Godspeed is a fork of Zeron, so the RPC protocol, the
client, and every call in the workload are identical. This does not measure
GUI rendering, window memory, or agent/model task speed. See
[Limitations](#limitations).

## Environment

| | |
|---|---|
| Machine | MacBook Air, Mac14,2, Apple M2 (4P + 4E cores), 16 GB |
| OS | macOS 27.0 (26A428), Darwin 27.0.0 |
| Godspeed | **0.2.0**, official release asset `godspeed-0.2.0-macos-arm64-app.tar.gz` from `ShriD5/godspeed-code` tag `v0.2.0` (`77b9540f`). `--version` prints `godspeed 0.2.0`. Ad-hoc signed. |
| Godspeed tarball SHA-256 | `4a412fa9820d3c9da9385f6dc65aa9ce7c6d3ca0e78824793eb40a42b7f58992` (matches the release's `SHA256SUMS`) |
| Godspeed binary SHA-256 | `e3c2b7ea87cd1bc39bb7c57bd7de7bb77b9935e347c2bee57ee9807f739b99d7` (148,005,680 B) |
| Zeron | **0.2.113**, official release asset `zeron-0.2.113-macos-arm64-app.tar.gz` from `zeronsh/zeron` tag `v0.2.113` (`6ac88599`, the fork point). `--version` prints `zeron 0.2.113`. Developer ID signed and notarized (team `5XY3M483YQ`). |
| Zeron tarball SHA-256 | `0e562e97aae4818b48559921054e243138b97ee99697ac0870bc1f916ba145ac` (matches the release's `manifest.json`) |
| Zeron binary SHA-256 | `29e4b0efaba058ecc6ebbb107221643733b2e1c9f2952ac50f66042766bb4e48` (141,062,032 B) |
| Installed Godspeed (extra arm) | `/Applications/Godspeed.app/Contents/MacOS/godspeed`, SHA-256 `29cf7cb8316b3ee34851494deaf3758f602160fd36061ca57e2bd7b66e71dedc`. Built 2026-10-16 16:02 IST. `--version` prints **`godspeed 0.2.113`**, not 0.2.0. |

**The installed app is not 0.2.0.** The brief described
`/Applications/Godspeed.app` as version 0.2.0. Its binary reports 0.2.113, and
`Info.plist` says the same. It was built at 16:02 IST, between `99bbede2`
(15:58) and the 0.1.0/0.2.0 release commits. It contains no Telegram code (0
`telegram` strings, against 60 in the 0.2.0 binary), and the Telegram bot
merged at 17:16–17:37. So the head-to-head uses the official 0.2.0 release
asset instead. That is the build users download, and it is the like-for-like
counterpart of Zeron's official release asset. The installed build ran as a
fourth arm so its numbers are on record too.

**Binary location.** Both release archives were downloaded and extracted into
their own new directories on the external drive:

- `bench-zeron/zeron-dl/`
- `bench-zeron/godspeed-dl/`

Both bundles were then copied with `ditto` to `/tmp/bench-bins/` on the
internal SSD, and the copies' hashes and signatures were re-verified. This
keeps USB disk reads out of cold start for both apps. The installed Godspeed is
on the internal SSD already. Zeron was never installed into `/Applications`,
and its GUI was never launched. Only `zeron headless` and `zeron --version`
ran.

**Machine state.** The run waited for the machine to go idle, with no
`rustc`/`cargo`/`xcodebuild`/`swift-frontend` and a 1-minute load under 4. It
polled every 60 s and timed out after 30 min, at 18:52. During that wait,
another agent's iOS and Rust builds ran, and the load reached 33. The builds
ended at about 18:56, and the first trial started then. No build process was
seen immediately before or after any trial. The 1-minute load at trial start
was 3.97–10.33 (median 6.2). Spotlight (`spotlightknowledged`) and
`mediaanalysisd` were busy at times. The owner's Godspeed GUI was open and used
roughly one core. The benchmark never touched its data or port.

## Method

The harness is `bench-zeron/bench.py`, derived from `../bench/bench.py`, the
Godspeed vs T3 harness. Its `GodspeedClient` is used unchanged for both apps.
Each trial:

1. Creates a fresh `mktemp -d /tmp/bench-<variant>-XXXX` holding `home/`,
   `data/`, `tmp/`, `project/` and `projects/`. The owner's `~/.godspeed`,
   `~/.zeron`, `~/.t3`, `~/.claude` and `~/.codex` are never read or written,
   and `~/.zeron` still does not exist. Picks a free loopback port.
2. Spawns `<binary> headless` with a scrubbed environment and
   `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so no agent CLI is reachable.
3. Measures **cold start**: from just before `Popen` until `EngineReady`
   returns on the IPC socket. "Port accepting" (first successful TCP connect)
   is reported separately.
4. Keeps one WebSocket client connected and idles:
   - At ready + 5 s and ready + 30 s, sums RSS (`ps`) and physical footprint
     (`footprint -f bytes --noCategories`) over the server's process tree.
   - Idle CPU is the tree's CPU-time delta (`ps time`) from ready + 10 s to
     ready + 30 s.
   - Lists the tree's non-loopback sockets (`lsof -nP -a -i -p`) at ready +
     1 s, + 6 s, + 30 s and after the workload. This is never during a timed
     section.
5. Runs the common workload. Each latency is one request/response round trip,
   timed with `perf_counter` in the client: 200 repetitions each, except
   `list_201p_400t` (50).
6. Re-samples memory 2 s after the last operation.
7. Shuts down with `StopEngine`, checks that nothing from the tree is left
   (nothing was), and deletes the temp dir (all were removed).

Before the first trial, `run_all.py` runs each binary once with `--version`,
untimed. In the pilot, the very first `headless` launch of each freshly
extracted binary took 2.0 s (Zeron) and 2.5 s (Godspeed), against about 60–90
ms afterwards. That looked like a first-launch code-signature assessment plus a
cold page cache, and both apps paid it equally. The pilot is in `pilot.jsonl`
and is not part of the results.

### Variants and schedule

| Variant | Binary | Extra environment |
|---|---|---|
| `godspeed` (G) | Godspeed 0.2.0 release | none |
| `zeron` (Z) | Zeron 0.2.113 release, **as shipped** | none (production edge `https://edge.zeron.sh`, baked WorkOS client id) |
| `zeron_noedge` (Zn) | same Zeron binary | `ZERON_EDGE_URL=https://edge.zeron.invalid` |
| `godspeed_installed` (Gi) | `/Applications/Godspeed.app` | none |

Each round runs `G Z Zn Gi Gi Zn Z G`, three times, for 6 trials per variant.
The G/Z pair by itself follows the requested `G Z Z G` pattern.

### Operations (identical for every variant)

| Name | RPC |
|---|---|
| `list_*` | `WatchSpaces` + `WatchChats`, pipelined; time until both initial snapshots arrive, then cancel |
| `create_thread` | `Mutate {op:createChat, spaceId}` |
| `create_project_thread` | `Mutate createSpace` then `Mutate createChat` (2 round trips) |

The store grows in this order: empty (`list_empty`) → 1 project with 200
threads (`create_thread`, `list_1p_200t`) → plus 200 project+thread pairs
(`create_project_thread`, `list_201p_400t`). All 24 trials ended with exactly
(1, 200) and then (201, 400) in the snapshots.

**Left out:** chat import (`RunChatImport`) and message search
(`SearchMessages`). Zeron 0.2.113 has neither RPC; neither name exists anywhere
in its source at the tag. Both phases were skipped for every variant, so the
workload and the post-workload memory sample are identical. Godspeed-only
import and search figures are in `BENCHMARKS.md` (Godspeed vs T3), measured on
an earlier build.

### Environment variables

These were checked against Zeron's source at `v0.2.113`, not guessed:

- `apps/zeron/src/main.rs` reads `ZERON_IPC_PORT`, `ZERON_HARNESS`,
  `ZERON_EDGE_URL`, `ZERON_EDGE_TOKEN`, `ZERON_WORKOS_CLIENT_ID` and
  `ZERON_ORG_ID`.
- `apps/zeron/src/paths.rs` reads `ZERON_DATA_DIR`.

```sh
# Godspeed (G and Gi)
env -i HOME=$T/home GODSPEED_DATA_DIR=$T/data GODSPEED_IPC_PORT=$PORT \
  GODSPEED_HARNESS=mock MUSE_EXECUTABLE=$T/no-provider/muse \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin SHELL=/bin/zsh TMPDIR=$T/tmp/ LANG=en_US.UTF-8 \
  /tmp/bench-bins/Godspeed-0.2.0.app/Contents/MacOS/godspeed headless

# Zeron (Z); Zn adds ZERON_EDGE_URL=https://edge.zeron.invalid
env -i HOME=$T/home ZERON_DATA_DIR=$T/data ZERON_IPC_PORT=$PORT \
  ZERON_HARNESS=mock MUSE_EXECUTABLE=$T/no-provider/muse \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin SHELL=/bin/zsh TMPDIR=$T/tmp/ LANG=en_US.UTF-8 \
  /tmp/bench-bins/Zeron.app/Contents/MacOS/zeron headless
```

`MUSE_EXECUTABLE` is not a `GODSPEED_*` variable. It blocks Godspeed's probe
of Homebrew's `muse` (see `BENCHMARKS.md`). Zeron 0.2.113 has no Muse harness
and ignores it. It is set for both so the two environments differ only in the
prefix. No other `GODSPEED_*` variable is set.

The full run is `python3 -I run_all.py`, then `python3 -I analyze.py`, from
`bench-zeron/`.

### Provider safety

No model provider was started. Both ran with the `mock` harness and the
restricted `PATH` with a temp `HOME`. The only child process seen in any trial
was `git`, from the engine's repository probe on the created projects. Both
apps have it.

## Does Zeron phone home?

**Yes, once, 20 s after startup. Nothing connects during startup itself.**

`netprobe.py` is untimed. It launches a variant with the trial environment and
calls `lsof` back to back (466–753 polls over 45 s) from spawn onward. Results:

| Variant | Non-loopback sockets in 45 s |
|---|---|
| Zeron as shipped (2 runs) | One TCP connection to `[2606:4700:3032::6815:10bc]:443` (Cloudflare, an `AAAA` record of `edge.zeron.sh`), first seen at 20.18 s and 20.35 s, `ESTABLISHED`, then closed |
| Zeron, `ZERON_EDGE_URL=https://edge.zeron.invalid` | none |
| Godspeed 0.2.0 | none |
| Godspeed installed build | none |

(Measured before Godspeed added its daily anonymous usage ping, which goes out about a minute
after launch unless turned off; see [PRIVACY.md](../PRIVACY.md).)

The source explains the timing:

- At `v0.2.113`, `zeron_engine` starts `zeron_update::Updater` for every
  install.
- Its engine role waits `ENGINE_INITIAL_DELAY = 20 s`, then fetches
  `{edge}/releases/manifest.json` (falling back to `latest.txt`).
- It repeats every hour (`CHECK_INTERVAL`), with user agent `zeron/0.2.113`.
- It only reports, and does not apply the update unless `ZERON_AUTO_UPDATE` is
  set.

From reading the code, the request is an unauthenticated release-feed GET. The
traffic itself was not captured, so the payload is unverified. No
WorkOS/sign-in or sync connection appeared. With no saved session, the baked
WorkOS client id did not open a socket.

Godspeed keeps the same updater, but its default edge is
`https://edge.godspeed.invalid`, so the check fails at DNS and never connects.
The "edge unresolvable" Zeron variant behaves the same way. `lsof` only shows
the process's own sockets. A DNS query goes through `mDNSResponder` and is
invisible here, for Godspeed and for Zeron-noedge alike.

The fixed `lsof` samples in the timed trials saw no external socket for any
variant. They fall at + 1, + 6 and + 30 s, and the update check is a short
connection at + 20 s. That check falls inside Zeron's idle-CPU window (+ 10 to
+ 30 s), but it did not register at `ps` resolution (see below).

## Results

Each cell gives the median across the 6 trials, with the min–max of the
per-trial values in parentheses. G/Z is Godspeed ÷ Zeron as shipped, so
values above 1.00x mean Godspeed is bigger or slower.

### Startup, memory, CPU, shutdown

| Metric | Godspeed 0.2.0 | Zeron 0.2.113 (as shipped) | G / Z | Zeron, edge unresolvable | G / Zn | Godspeed installed (0.2.113-labelled) |
|---|---:|---:|---:|---:|---:|---:|
| Spawn → port accepting (ms) | 64.8 (58.9–119.5) | 64.7 (51.6–116.2) | 1.00x | 61.1 (53.6–66.5) | 1.06x | 68.8 (52.9–141.2) |
| Spawn → `EngineReady` answered (ms) | 65.2 (59.3–124.3) | 65.1 (52.1–120.2) | 1.00x | 61.5 (54.2–67.1) | 1.06x | 69.4 (53.3–161.4) |
| RSS, ready + 5 s (MiB) | 32.3 (32.1–32.4) | 28.4 (28.3–28.7) | 1.14x | 28.5 (28.3–28.7) | 1.13x | 31.7 (31.6–31.8) |
| Physical footprint, ready + 5 s (MiB) | 11.40 (11.19–11.45) | 9.45 (9.39–9.78) | 1.21x | 9.58 (9.41–9.72) | 1.19x | 11.13 (11.00–11.27) |
| RSS, ready + 30 s (MiB) | 34.6 (34.2–34.8) | 31.9 (31.6–32.0) | 1.09x | 30.9 (30.5–31.0) | 1.12x | 34.0 (33.9–34.2) |
| Physical footprint, ready + 30 s (MiB) | 12.20 (11.74–12.28) | 10.59 (10.31–10.70) | 1.15x | 10.43 (10.06–10.56) | 1.17x | 11.87 (11.78–11.99) |
| Processes in tree, ready + 30 s | 1 | 1 | | 1 | | 1 |
| Idle CPU, ready + 10 s → + 30 s (% of one core) | 0.05 (0.05–0.05) | 0.05 (0.05–0.05) | | 0.05 (0.05–0.05) | | 0.05 (0.05–0.05) |
| RSS after workload (MiB) | 87.5 (82.6–108.4) | 82.9 (79.2–92.7) | 1.06x | 85.4 (82.7–89.7) | 1.02x | 89.5 (84.2–98.7) |
| Physical footprint after workload (MiB) | 33.9 (31.0–44.5) | 33.2 (30.9–36.7) | 1.02x | 36.0 (31.0–39.0) | 0.94x | 34.2 (32.1–36.4) |
| Processes in tree after workload | 2 (engine + `git`) | 2 | | 2 | | 2 |
| `StopEngine` → exit (ms) | 1651 (1052–1871) | 1522 (613–1941) | 1.08x | 1766 (657–2430) | 0.93x | 1930 (736–5965) |
| 1-min load at trial start | 5.49 (4.77–8.35) | 6.23 (4.60–10.33) | | 6.66 (5.81–7.05) | | 6.88 (3.97–8.39) |

Notes on this table:

- **Idle memory is the one clear, consistent difference.** Godspeed 0.2.0's
  idle physical footprint is 1.6 MiB higher than Zeron's at ready + 30 s, and
  2.0 MiB higher at + 5 s. Its RSS is 2.7–3.9 MiB higher. The ranges do not
  overlap in any of the six trials. This fits Godspeed's extra resident
  subsystems:
  - always-on agent scheduler
  - Telegram bot
  - message index
  - limit recovery
  - delegation outbox

  The installed pre-Telegram build sits about 0.3 MiB below 0.2.0. The
  edge-less Zeron is no lighter than Zeron as shipped, so the gap is not the
  edge client.
- **Cold start is a tie.** Both medians are about 65 ms, with overlapping
  ranges. Every variant had one or two slow outliers (116–161 ms), as in the
  T3 run; the cause was not isolated.
- **After the workload, memory is within noise** (33.9 vs 33.2 MiB, ranges
  31.0–44.5 vs 30.9–36.7). The fixed idle overhead is small next to the
  per-chat data.
- **Idle CPU** is one 10 ms `ps` tick over the 20 s window for every variant,
  which is the floor of this method. Zeron's update check at + 20 s did not
  register above it.
- **Shutdown** took 0.6–2.4 s for both apps, and once 6.0 s for the installed
  Godspeed. The spread is wide and overlapping, and it is higher than the
  0.6–1.0 s seen for Godspeed in the T3 run on a quieter evening. Neither app
  is meaningfully faster. All 24 exits were graceful (return code 0), with no
  leftover processes.
- The post-workload footprint covers the engine process only. The `git` child
  was gone or a zombie (`<defunct>` once, for Zeron) by the time `footprint`
  ran, so the tree sum and the engine figure are the same.

### Request latency (ms)

Each cell is the median of the 6 per-trial p50 (or p95) values, with the
min–max across trials in parentheses.

| Operation | Godspeed p50 | Godspeed p95 | Zeron p50 | Zeron p95 | G/Z p50 | Zeron (no edge) p50 | Zeron (no edge) p95 | G/Zn p50 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `list_empty` | 0.140 (0.124–0.193) | 0.184 (0.161–0.210) | 0.133 (0.125–0.177) | 0.173 (0.141–0.210) | 1.05x | 0.149 (0.132–0.171) | 0.182 (0.169–0.192) | 0.94x |
| `create_thread` | 0.215 (0.180–0.529) | 0.370 (0.334–0.914) | 0.208 (0.187–0.369) | 0.381 (0.352–0.700) | 1.04x | 0.255 (0.195–0.369) | 0.495 (0.329–0.662) | 0.84x |
| `list_1p_200t` | 0.694 (0.625–1.722) | 0.769 (0.670–1.839) | 0.663 (0.562–1.082) | 0.718 (0.620–1.244) | 1.05x | 0.829 (0.564–1.066) | 0.901 (0.624–1.154) | 0.84x |
| `create_project_thread` | 1.803 (1.477–4.864) | 3.027 (2.046–29.47) | 1.848 (1.456–3.096) | 3.037 (2.126–26.16) | 0.98x | 2.042 (1.464–2.844) | 3.043 (2.142–8.913) | 0.88x |
| `list_201p_400t` (50 reps) | 1.948 (1.279–4.058) | 2.325 (1.385–6.682) | 1.742 (1.142–3.431) | 2.091 (1.266–4.637) | 1.12x | 1.965 (1.135–3.748) | 3.451 (1.289–29.95) | 0.99x |

Installed Godspeed build, p50 / p95:

| Operation | p50 | p95 |
|---|---:|---:|
| `list_empty` | 0.142 | 0.179 |
| `create_thread` | 0.267 | 0.473 |
| `list_1p_200t` | 0.888 | 1.014 |
| `create_project_thread` | 2.184 | 3.911 |
| `list_201p_400t` | 2.911 | 7.831 |

Ranges are in `summary.json`.

**No latency difference is distinguishable from noise.** For every operation,
the per-trial ranges overlap heavily:

- Against Zeron as shipped, Godspeed is 2–12% slower at the median on four
  operations and 2% faster on one.
- Against the same Zeron binary with the edge off, Godspeed is 1–16% faster
  on every operation.

So the two Zeron configurations differ from each other by about as much as
Godspeed differs from either. The p95 tails (up to 30 ms) are scattered across
all variants and follow machine load, not the app. Neither engine's list cost
grows much with project count (about 0.7 ms at 1 project, about 2 ms at 201).
Unlike T3, there is no per-project snapshot cost.

### Static footprint

| | Godspeed 0.2.0 | Zeron 0.2.113 | G − Z |
|---|---:|---:|---:|
| App bundle on disk (`du -sk`) | 143.8 MiB (147,268 KiB) | 137.0 MiB (140,268 KiB) | +6.8 MiB (+5.0%) |
| App bundle, sum of file sizes | 150,778,094 B | 143,612,962 B | +7,165,132 B |
| Main executable (arm64 Mach-O, UI + engine in one binary) | 148,005,680 B | 141,062,032 B | +6,943,648 B (+4.9%) |
| Release archive (`*-macos-arm64-app.tar.gz`) | 52,482,785 B | 49,387,270 B | +3,095,515 B |

The installed Godspeed bundle is 142.9 MiB (`du -sk` 146,376 KiB), and its
executable is 147,092,912 B. Godspeed's tag is 158 commits past the fork, with
about 50.8k inserted lines in `crates/` and `apps/`.

## Raw data

Everything is in `/Volumes/Lexar/build/godspeed-coordination/bench-zeron/`, on
the owner's external drive, outside the repo:

- Results:
  - `results.jsonl`: one record per trial, with every sample, per-pid
    footprints, process names, latencies, socket samples, network-related log
    lines, shutdown info, load before/after, and build processes before/after.
  - `results.csv`: one row per trial.
  - `summary.json`: includes the per-trial values.
  - `environment.txt`: OS, hardware, sizes, hashes, signatures, versions.
  - `idle_gate.json`, `run_all.log`
  - `netprobe/*.json`: the four network-probe runs.
- Scripts:
  - `bench.py`: one trial.
  - `run_all.py`: idle gate, warm-up, interleaved schedule.
  - `analyze.py`
  - `netprobe.py`
  - `rawws.py`: unchanged from `../bench`.
- `pilot.jsonl`: one or two pilot trials per variant, used to debug the
  scripts. It is not included in the results.
- Binaries:
  - `zeron-dl/` and `godspeed-dl/` hold the untouched downloads and their
    extracted bundles.
  - `/tmp/bench-bins/` holds the internal-SSD copies that actually ran. They
    are safe to delete.

## Limitations

- **Headless engine only.** There is no GUI. Neither app's GPUI window ran, so
  this says nothing about rendering, input latency, or memory as a user
  normally runs either app. For both apps, the headless engine and the headed
  app are the same binary.
- **No agent work.** No provider, model call or turn ran. Nothing here
  measures task speed, streaming, or provider processes. Godspeed's extra
  subsystems were present but mostly idle: no Telegram token, no scheduled
  agents, no index to build. Their cost under real use is not measured. Only
  their resident idle cost is (about 1.6–2 MiB).
- **Common workload only.** Import and search were not run because Zeron
  cannot do them. A "same workload" comparison leaves out the features that
  make Godspeed different.
- **Which Godspeed.** The head-to-head uses the official 0.2.0 release asset,
  not the `/Applications` app named in the brief. The installed app is a
  16:02 IST build that reports 0.2.113 and predates the Telegram bot. Its
  numbers are reported as a separate arm. The 0.2.0 release binary also
  differs from the local `target/release/godspeed` (SHA-256 `7c26ff79…`, built
  17:51), so it is not a byte-for-byte local build.
- **Signing differs.** Zeron is Developer ID signed and notarized. Godspeed is
  ad-hoc signed (`spctl` rejects it). The first-launch assessment cost was
  absorbed by a warm-up run and excluded. Signing could still affect launch
  paths in ways not isolated here.
- **Zeron as shipped vs edge-off.** The edge-off variant only changes
  `ZERON_EDGE_URL`. `ZERON_WORKOS_CLIENT_ID` still had its baked default, while
  Godspeed has no WorkOS client by default. No socket was attributable to
  WorkOS, but the two are not configured identically.
- **Network check method.** `lsof` polling (about every 60–100 ms) sees TCP
  connections owned by the process. It would miss a connection that opens and
  closes between two polls, and it never sees DNS (done by `mDNSResponder`) or
  payloads. The Zeron connection matched the 20 s update-check timer exactly,
  in 2 of 2 runs. Nothing was packet-captured.
- **Machine noise.** The 30-minute idle gate timed out while another agent was
  building. Trials started once its builds ended, and none overlapped a build
  process. The 1-minute load was still 4.0–10.3, with Spotlight, media
  analysis and the owner's Godspeed GUI active. Interleaving spreads this
  across variants. Memory figures are tight. Cold-start outliers, p95 tails
  and shutdown times are visibly affected. Absolute numbers on an idle machine
  would likely be lower.
- **Client cost.** The client is synchronous Python, so its overhead (tens of
  microseconds) is in every latency, equally for every variant.
- **Sample size.** Six trials per variant. Ranges are reported instead of
  confidence intervals. Differences under about 15% in latency, startup or
  shutdown should be read as ties.
