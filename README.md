# zed-intellij

A [Zed](https://zed.dev) extension that runs JetBrains' standalone IntelliJ
language server — **"Java and Kotlin by IntelliJ IDEA"** (`intellij-server`) —
for Java and Kotlin buffers, including the **IntelliJ debug engine** the server
embeds behind its `start_debug_server` LSP command.

One language server (`intellij-server`) serves both languages from the same
engine and project model. The debug adapter (`intellij_jvm`) is a small bridge
process that pumps DAP between Zed and the TCP endpoint the server opens, so
Kotlin + Java breakpoints and mixed-source stepping work against IntelliJ's own
project model (mainClass → classpath resolution happens inside the server).

## Install

This is a machine-local extension, not (yet) in the Zed registry. In Zed:
` Extensions panel → Install Dev Extension → select this repo` (requires the
Rust `wasm32-wasip1` target). Grammars come from Zed's official `kotlin`/`java`
extensions — install this alongside them.

### Accepting the JetBrains license (first run)

Nothing is downloaded until you accept JetBrains' license. On first launch the
language server reports a failure pointing back here instead of fetching the
distribution; opt in by adding this to `settings.json` and reloading the window:

```json
{
  "lsp": {
    "intellij-server": {
      "settings": {
        "accept_jetbrains_eula": true
      }
    }
  }
}
```

The server is proprietary JetBrains software governed by the **JetBrains User
Agreement v2.0**, which ships as `EULA.txt` inside the downloaded bundle.
Acceptance is recorded with the launcher by passing
`--eula=<sha256(EULA.txt)[:16]>`; that hash is derived from the EULA.txt of the
build you actually have, so it tracks version bumps instead of being pinned.
Only a literal JSON `true` enables the download — a string `"true"` does not.

### Native binaries

Two native helpers are needed; the extension resolves them in this order (see
`src/language_servers/util.rs`):

1. `bin/<exec>` — local dev override (e.g. your own build of `proxy/` /
   `bridge/`),
2. `bin/<version>/<exec>` — extracted from the release archive,
3. **on demand**: `zed-intellij-v<version>-<target-triple>.tar.gz` downloaded
   from the GitHub release tagged `v<version>` (built by
   `.github/workflows/release.yml`) and extracted into `bin/<version>/`.

- `kotlin-lsp-proxy` (from `proxy/`) sits on the LSP pipe and does two things:
  1. rewrites the archive-internal (`jar!/`, `zip!/`) source URIs the server
     returns into real files so Go-to-Definition into library/JDK sources works;
  2. repairs outgoing `textDocument/codeAction` requests (see below), without
     which the server's "Import class …" / "Create class …" quickfixes can go
     missing.
  Optional — without the proxy the server still runs, but those quickfixes stay
  missing in that case.
- `intellij-dap-bridge` (from `bridge/`) is the debug adapter. Required for
  debugging (Unix sockets; no Windows).

#### Why the proxy repairs code-action requests

intellij-server builds its unresolved-symbol quickfixes **only** from the
diagnostics the client echoes back in `context.diagnostics`: the Java/Kotlin
code-action providers deserialize each incoming diagnostic's opaque `data`,
recompute the file's diagnostics, match the two, and list the fixes stored in
that `data`. Handed an empty `context.diagnostics` they return *no* quickfixes at
all — the user sees only unrelated actions (typically "Organize imports")
although completion inserts the very same import.

Zed builds that list in `GetCodeActions::to_lsp` with
`entries_in_range(self.range, buffer, inclusive = false, reversed = false)`,
whose overlap test is *strict*: `start < entry.max_end && end > entry.min_start`.
The request range is the selection — usually a zero-width caret — so a caret that
sits exactly on a diagnostic's first or last character (or otherwise not
strictly inside the published span) drops that diagnostic from the request. The
server is then handed `diagnostics: []` and offers nothing. That is exactly why
the same import works from completion, and why only servers that derive their
fixes from `context.diagnostics` (rather than recomputing them from the request
range) are affected.

(The request is *not* losing each diagnostic's `data`: Zed round-trips that
faithfully, verified against captures of real Zed requests. The failure is the
diagnostic itself being omitted.)

The proxy caches the diagnostics the server published — push
(`publishDiagnostics`) and pull (`textDocument/diagnostic`) alike — and repairs
outgoing code-action requests in two additive steps:

1. **add back** published diagnostics that overlap the request range (inclusive,
   so a caret on the diagnostic's edge still counts) but are missing from
   `context.diagnostics`;
2. **fill in** a missing `data` on a diagnostic the client did send, for clients
   that do not round-trip it.

Neither step removes or overwrites what the client sent, and both are no-ops when
the request is already correct — such requests are forwarded byte-for-byte.

#### Shipping proxy/bridge changes

Nothing to reseed by hand: a dev-extension reinstall wipes `bin/`, and the
extension then re-downloads `zed-intellij-v<version>-<target-triple>.tar.gz` from
the GitHub release tagged `v<version>`. So a change to `proxy/` or `bridge/`
reaches Zed only once that release exists:

```sh
# bump Cargo.toml + extension.toml to the same version, then
commit … && git push origin main
git tag v0.2.0 && git push origin v0.2.0   # CI attaches the archives
```

A local `bin/<exec>` still wins over the downloaded copy (dev override), so a
local `cargo build --release` in `proxy/` can be tried before tagging. The
caveat: the pinned tag is `v<CARGO_PKG_VERSION>`, so until a release is tagged
for the current version, a reinstall silently falls back to the *previous*
release's binaries.

### Requirements

- The server distribution is downloaded at runtime from JetBrains: build
  `271.2085.0` (product version 2027.1). It is stable but **requires an
  IntelliJ IDEA Ultimate license** after a 30-day trial (enforced by the
  launcher, like the EULA).
- `JAVA_HOME` / JDK resolution (used for the server's `JAVA_HOME`, its Maven
  import forks via `JB_MAVEN_JAVA_HOME`, and `defaultSdk`):
  1. `lsp.intellij-server.settings.java_home` per project,
  2. `$JAVA_HOME` from your shell environment,
  3. the JDK bundled with the server distribution (`jbr/`), for machines with
     neither of the above.
- `lsp.intellij-server.settings.accept_jetbrains_eula` must be `true` before
  the server is downloaded or executed (see *Accepting the JetBrains license*).
- `lsp.intellij-server.settings.eula_hash` overrides the acceptance hash. By
  default it is computed from the `EULA.txt` of the installed distribution, so
  it follows the pinned build; the constant in
  `src/language_servers/intellij_server.rs` is only a fallback for the window
  before a distribution has been extracted.
- `lsp.intellij-server.settings.vm_options` tunes server JVM flags;
  `lsp.intellij-server.settings.proxy_path` overrides the proxy binary.
- Telemetry is off unconditionally (`INTELLIJ_DATA_SHARING=none`). Unlike the
  JetBrains client there is no opt-in setting: accepting the EULA is one axis,
  sending usage data is another, and this extension only ever does the first.

### Monorepos and build tools

The server imports the worktree root as one project unless it is told
otherwise, so in a monorepo the modules below the root resolve against a model
built from the root alone. Two settings map onto the same initialization
options JetBrains' own VS Code client sends:

```json
{
  "lsp": {
    "intellij-server": {
      "settings": {
        "intellij.projects": [
          { "type": "gradle", "path": "." },
          { "type": "maven", "path": "services/api" }
        ],
        "intellij.buildTool": "gradle"
      }
    }
  }
}
```

- `intellij.projects` — array of `{ type, path }` entries with `path` relative
  to the worktree root. This is what makes nested build files visible.
- `intellij.buildTool` — `"gradle"`, `"maven"`, `"bazel"`, or `""` to disable
  detection. Forwarded as a per-folder map keyed by the worktree's `file://`
  URI, so it applies to this project only; an empty string is sent deliberately,
  since it is how detection gets turned off. `buildTool` without the prefix is
  accepted as an alias. Only needed when detection picks the wrong tool or finds
  none.

## Disclaimer

This project is **not affiliated with, endorsed by, or sponsored by JetBrains
s.r.o.** "IntelliJ" and "IntelliJ IDEA" are trademarks of JetBrains s.r.o. The
intellij-server distribution is proprietary JetBrains software governed by
JetBrains' own license terms; this extension only downloads it from JetBrains
at runtime and never redistributes it.

## Attribution

Derived in part from [zed-extensions/kotlin](https://github.com/zed-extensions/kotlin)
(MIT, © 2024 Evren Sen) — the extension scaffolding and the
`kotlin-lsp-proxy` source-uri rewriting approach (kotlin#106).

## License

MIT — see [LICENSE](LICENSE).
