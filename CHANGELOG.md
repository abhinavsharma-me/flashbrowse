# Changelog - flashbrowse

All notable changes. Format: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

## [1.2.1] - 2026-11-23

### Changed
- `flashbrowse update` now downloads the matching platform release archive from
  GitHub Releases and replaces the running executable in place. Pass `--check`
  to only check and report without modifying the installation.

### Fixed
- Windows persistent `--session-id` launches no longer inherit the CLI's
  stdout/stderr pipe handles, preventing callers like `Command::output()` from
  hanging until Chromium exits. `close --session-id` now terminates the Windows
  browser process tree cleanly (`/T`) with null stdio.

## [1.2.0] - 2026-11-23

### Added
- Child `<iframe>` accessibility trees are now spliced directly into the root
  page AX tree in document order. Discovery walks the frame hierarchy via
  `Page.getFrameTree` and discovers child frames, giving agents visibility and
  action references into buttons, inputs, links, and text inside iframes (Fixes #2).
- Out-of-process iframe (OOPIF) coordinate translation: clicks and interactions
  targeted at child frame nodes calculate layout offsets from the frame's
  bounding box and route clicks to their exact viewport coordinates.
- Recursive markdown extraction: `browser_markdown` extracts content from child
  frames and appends them in document order, including iframe titles and text.

### Fixed
- Linux persistent sessions (`--session-id`) no longer terminate when the
  spawning CLI process exits. `PR_SET_PDEATHSIG` is skipped for persistent
  sessions so background browser processes survive across independent command
  invocations (Fixes #1).
- Windows PowerShell installer (`install.ps1`) resolves the latest GitHub
  release redirect reliably when determining the download version tag (PR #3).
- Install documentation updated to list valid, existing installation channels
  only (PR #4).

## [1.1.0] - 2026-11-23

### Added
- `dump --format raw` prints the uncompressed `Accessibility.getFullAXTree`
  JSON, so the compressor's input can be compared with its output. The AX
  protocol types now serialize with the same camelCase field names Chromium
  uses, and a round-trip test guards that.
- Sliders and spin buttons render their current position as `value=...`.
  Without geometry, that number is the only way an agent can reason about
  where a slider sits.
- A fidelity suite compares the compressed tree with the raw AX tree across
  forms, tables, accessible names, unicode, shadow DOM, frames and a spatial
  page. It asserts text recall, action recall, document order, no duplication,
  hidden-text exclusion, state round-trip and the 120-character truncation
  bound. `scripts/fidelity.mjs` runs the same checks against real URLs.
- Render modes for eval: `SemanticNode::depth`, `render_nested`,
  `dump --format axtree-nested`, and `dump --no-prune` (full non-ignored AX
  tree). `compress_with_options` exposes the pruned vs full split.
- Dev-only task eval under `eval/`: pilot tasks, MCP runner, Playwright
  success checks, and `report.mjs` for markdown summaries.
- MCP `browser_evaluate` runs JavaScript on the live session so task success
  checks use the same page the agent mutated, not a fresh browser tab.
- ARIA `gridcell` support in `compressor.rs` (`role="grid"` / `role="gridcell"`
  rows now collapse into `Row: ... | [N]` lines without duplicate `Gridcell:`
  output).
- Brand icon (`assets/icon.png`, `assets/icon.svg`).

### Fixed
- `checked`, `selected` and `expanded` were missing from the tree. Chromium
  sends `checked` as the string `"true"` rather than a JSON bool, and extras
  were only rendered on actionable lines, so a checked checkbox and a selected
  option looked exactly like their unselected siblings.
- An input value appeared twice (`value="Ada"` plus `Text: Ada`), and an icon
  link's `alt` text appeared twice (the link name plus an image line).
- Long or wide table rows (`> 120` chars) no longer truncate trailing `[N]`
  action references off the `Row:` summary line; `truncate_row` budgets
  non-ref cells while preserving `[N]` refs.

## [1.0.2] - 2026-11-22

### Added
- `open`, `dump`, `bench` and MCP `browser_open` accept a scheme-less URL.
  `flashbrowse open example.com` now works like an address bar: `https://` is
  assumed, local hosts (`localhost:3000`, `127.0.0.1`, `::1`, `*.local`,
  RFC1918 and link-local addresses) use `http://`, and an explicit scheme is
  never rewritten. Before this, a bare host reached `Page.navigate` verbatim
  and Chrome answered `-32000: Cannot navigate to invalid URL`.
- Input that cannot be a URL now fails before a browser launches, with the fix
  in the message: a bare path suggests `file:///`, a search query says
  flashbrowse opens URLs, and an unsupported scheme (`javascript:`,
  `mailto:`) is named. The MCP allowlist still runs on the normalized URL, so
  `file://` remains CLI-only.

## [1.0.1] - 2026-11-22

### Added
- `ResourcePolicy` for network blocking: `agent-default` (default), `text-only`,
  `full-render` (loads images/fonts; tree tokens unchanged). CLI:
  `--resource-policy`, `--full-render` (conflicts with `--text-only`), env
  `FLASHBROWSE_RESOURCE_POLICY`.
- Trustworthy `browser_screenshot`: lifts fonts and images during prepare,
  network-idle wait, lazy-content scroll, configurable `timeout_ms` / MCP
  `prepare_lazy_content`. Use the same flashbrowse session instead of a second
  browser MCP.
- MCP `browser_open` accepts `resource_policy`; tool descriptions steer agents
  to `browser_screenshot` on the active session.
- `CdpDriver` seam in `flashbrowse-core` for future engines.
- `flashbrowse-bidi` crate: Firefox binary discovery (`find_firefox`, doctor
  informational check); full BiDi sessions are not wired yet.
- **Observability**: every CLI command and every MCP tool call appends a
  record to `~/.cache/flashbrowse/metrics.jsonl` (op, target, duration, agent
  payload tokens, action count, ok/error).
- `flashbrowse stats [--limit N] [--json]`: latency percentiles (p50/p95),
  token totals, action averages and error rate over the metrics trail.
- `flashbrowse doctor [--json]`: environment diagnostics (Chromium presence
  and version, managed Chrome, cache-dir writability, free temp space,
  available memory, stored sessions, orphaned browser processes). Exits `1`
  when a blocking check fails.
- `flashbrowse update [--check]`: update-channel aware release check.
  Channel selected via `FLASHBROWSE_UPDATE_CHANNEL` = `stable` (default) |
  `beta` | `nightly` | `off`.
- `--log-format text|json` global flag: JSON logs are one object per line,
  ready for a log shipper. Logs always go to stderr (stdout stays pure
  JSON-RPC for MCP).
- `scripts/soak.mjs`: soak/load harness that drives a long-running MCP
  session and reports throughput, per-tool latency, first-tenth vs
  last-tenth drift, RSS growth, orphan-browser count and error list. CI runs
  a 5-minute version on every push.
- CI caches the managed Chrome-for-Testing download, keyed by the exact
  Chrome version, so the browser is fetched once instead of once per job.
- `FLASHBROWSE_CHROME_VERSION` pins the Chrome-for-Testing version used by
  `flashbrowse setup`.
- CDP `dispatch_frame` unit tests; WebSocket connect retries on early Chrome
  startup races.
- Soak exit checks for latency drift (> 1.2x) and flashbrowse RSS growth
  (> 1.5x).
- CI uploads `doctor --json` on Linux.

### Fixed (screenshot, policy, navigation)
- `browser_screenshot` no longer burns the full prepare timeout on a page whose
  images are blocked. The readiness check required `naturalWidth > 0`, which a
  permanently broken image never reaches; it now waits on `img.complete`
  (loaded *or* failed). A screenshot of an image-bearing page went from
  ~8.2 s to ~0.4 s.
- Blocked images are now actually rendered. Lifting the blocklist alone did
  nothing: `Network.setBlockedURLs` only affects *future* requests and Chrome
  never retries a blocked one. The capture now re-requests the images, so an
  agent-default screenshot shows the same pixels as a `full-render` one.
- The screenshot prepare no longer resets the scroll position. The agent asked
  for a picture of what it was looking at; the viewport is now restored after
  the lazy-content scroll.
- The screenshot prepare timeout is capped (500 ms - 60 s). An agent could
  previously ask for an unbounded wait and pin the serial MCP server.
- An invalid MCP `resource_policy` is now an `InvalidParams` error instead of
  being silently ignored (the CLI already rejected it).
- Resource-policy flag precedence is explicit and deterministic:
  `--resource-policy` > `--full-render`/`--text-only` > env > default.
  Conflicting flags now fail loudly.
- `doctor` no longer spawns `firefox --version`; it only reports the path.
- Plain text in generic containers (`<div>`, `<span>`) is no longer dropped.
  Chromium gives every `StaticText` an `InlineTextBox` child; treating that as
  "has children, so it is a concatenation duplicate" made the compressor drop
  every piece of plain text on the page. `InlineTextBox` is now recognized as
  layout scaffolding, and `StaticText` renders as a `Text:` line.
- A minimal page (one or two semantic nodes) no longer waits out the whole
  15 s settle deadline: the exit condition required `nodes.len() >= 3`, which
  a tiny page never reaches. Such pages now open in ~0.6 s instead of ~16 s.
- The `Title:` header no longer comes up blank about one page in five. The
  title was read right after `navigate`, before the new document was parsed;
  it is now re-read once the page has settled.
- Content arriving shortly after `readyState == "complete"` (hydration
  timers up to ~1.2 s) is no longer truncated: the small-tree quiet window
  grew from 800 ms to 1200 ms.

### Fixed (accuracy)
- **Table contents are no longer dropped.** Table rows are now emitted as one
  line per row (`a | b | c`) instead of one line per cell, and the row's own
  accessible name (empty in Chromium) no longer hides the cell text.
- **List item text is no longer dropped.** A `listitem` keeps its text.
- **Output is in document order.** Chromium returns the accessibility tree
  breadth-first, which interleaved siblings with their descendants (a table
  header row could arrive after its body rows). The compressor now rebuilds
  the tree and walks it depth-first.
- An actionable element inside a container no longer causes its text to be
  emitted twice.
- `listmarker` bullet glyphs are dropped (decoration, not content).
- Unnamed links no longer occupy action IDs in the action map when dropped.

### Fixed
- `doctor` memory and orphan-browser checks on macOS and Windows.
- Metrics `record_op` for `setup`, `close`, `bench`, and failed `act` paths.
- CI soak job Chrome cache key (`ubuntu-latest`, not empty `matrix.os`).
- The Windows build did not compile: `tokio::signal::unix` was called
  unconditionally, so `cargo build` and clippy died with E0433 and a cascading
  E0599. Signal handling is now unix-only, with console Ctrl+C on Windows;
  `libc` is a unix-only dependency and the `ps`-only orphan scan helper is
  gated so clippy `-D warnings` passes on every OS.
- An explicit Firefox path is authoritative. `find_firefox(Some(p))` used to
  fall back to PATH when `p` did not exist, which silently ran a different
  browser than the caller pinned.
- The soak memory gate no longer fails on allocator warm-up. It compared the
  first RSS sample against the last, so a cold process climbing from 11 MB to
  18 MB in its first 30 s read as a leak; the 5 minute CI soak failed on a run
  whose RSS was flat for the remaining 4.5 minutes (and ended below its own
  30 s peak). The gate now baselines after a warm-up window and fails only on
  real growth: a 1.5x ratio with an 8 MiB floor, or a fitted trend (R2 >= 0.8,
  which a one-time step cannot reach) that would exceed 1.5x within an hour.
  `node scripts/soak.mjs --check-report <file>` re-runs the gates on a saved
  report.

### Security
- `url` 2.5.8 carried RUSTSEC-2026-0592: `Url::origin()` recursed once per
  `blob:` prefix with no depth limit, so a deeply nested `blob:` URL overflowed
  the stack and aborted the process. Bumped to 2.5.9, which takes the inner
  origin only for `http`, `https` and `file` and never recurses
  (`form_urlencoded` 1.2.3 comes with it). `url` arrives transitively through
  `reqwest`; flashbrowse never calls `Url::origin()`, but the bump keeps
  `cargo audit` clean and costs nothing.
- `zip` 4.6.1 carried six advisories (RUSTSEC-2026-0429, -0367, -0548,
  -0368, -0369, -0545). Upgraded to 9.8.1 with only the deflate feature
  enabled, since Chrome-for-Testing archives are deflate/store and the AES,
  LZMA, Zstd, bzip2 and ppmd decoders were never exercised. `cargo audit` is
  clean.

### Changed
- License is **Apache-2.0** (the same license as Playwright and Puppeteer).
  `LICENSE` added; `Cargo.toml` and the README updated.

## [1.0.0] - 2026-11-14

First public release. Single Rust binary, zero Python/Node/JVM, raw CDP over
WebSocket, AXTree-first compression, native MCP stdio server.

### Added
- `flashbrowse setup`: managed Chrome-for-Testing download
  (`~/.cache/flashbrowse/chrome/`, ~120 MB, once) - no manual Chrome install.
- MCP stdio server (protocol 2025-06-18, falls back to 2025-03-26 / 2024-11-05)
  with four tools: `browser_open`, `browser_act`, `browser_extract_markdown`,
  `browser_screenshot`.
- CLI: `mcp`, `open`, `act`, `dump`, `bench`, `setup`, `close` with
  `--chrome-path`, `--text-only`, `--session-id` persistence.
- `select` action (combobox/dropdown) via `DOM.resolveNode` +
  `Runtime.callFunctionOn` - never opens the native popup.
- Actionability checks for clicks (visible + stable, Playwright parity).
- Adaptive settle engine: cheap DOM polling, network-idle gate, AX-vs-DOM
  readiness gate and content-fingerprint verification, so the tree is never
  truncated (byte-identical output across repeated opens).
- Network interception: fonts, trackers and images blocked by default
  (image `alt` text survives in the AX tree); CSS deliberately not blocked.
- Stealth script: `navigator.webdriver`, `window.chrome`, plugins, mimeTypes,
  languages, platform/vendor, CPU/memory, permissions, WebGL masking.
- One-script installers (`install.sh`, `install.ps1`), CI on Linux/macOS/Windows,
  release workflow for 5 targets.

### Fixed (correctness)
- `AXNode` now matches the CDP schema exactly (`nodeId`, `backendDOMNodeId`,
  `value`, `ignoredReasons`, `properties`) - the old mapping silently dropped
  every action ID.
- Heading levels read from `properties` (was always 1).
- macOS select-all uses Cmd (Meta=4), not Shift (8).
- Key events send the DOM `code` (`a` -> `KeyA`).
- A malformed `tools/call` no longer terminates the MCP server.
- Stealth and network blocking are re-applied on session re-attach (they are
  per-`sessionId` settings).
- `select` no longer leaves the native dropdown open.
- Profile directories are removed reliably (synchronously, not in a thread
  that process exit could kill).
- `close` verifies the pid still belongs to Chrome before killing it; corrupt
  session files are cleaned instead of blocking cleanup.
- Invalid session ids are rejected before detaching (no orphaned browser).
- Oversize CDP frames fail their pending calls instead of timing out.

### Performance
- 3x - 23x fewer agent tokens than Playwright/Puppeteer accessibility
  snapshots (measured against the full tree, `interestingOnly: false`).
- 1.08x - 1.10x faster than Playwright/Puppeteer on the agent loop
  (524 ms vs 564/579 ms median, local page, same Chrome).
- 98.8% token reduction on a 50k-node synthetic AX tree (182 ms).
- 234 ms average for 20 sequential opens, 286 ms for a 100-action storm.

> The agent payload grew from 813 to 1215 tokens on the local benchmark page
> because the compressor stopped dropping table cells. The old number was
> smaller because the data was missing.

### Known limits (documented in README)
- Stealth defeats naive bots, not canvas/audio/TLS fingerprinting.
- MCP `browser_open` is URL-allowlisted (`http`, `https`, `data`, `about:blank`).
- Markdown extraction is lightweight, not a full Readability port.
- macOS/Windows binaries are ad-hoc signed; notarization needs an Apple
  Developer certificate (see `RELEASE.md`).
