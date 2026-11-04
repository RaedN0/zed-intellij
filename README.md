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

#### One-time local steps

The packed distro download is skipped when
`bin/<version>/` already exists, and the packed release
binary stays as it was until a new archive is tagged. On this machine the fix is
already live: the rebuild lives at `bin/kotlin-lsp-proxy` (dev override). After a
dev-extension reinstall wipes `bin/`, rebuild and reseed:

```sh
(cd proxy && cargo build --release)          # rebuild the proxy
~/.local/share/zed-kotlin-patches/reseed-workdir.sh
```

To publish new binaries: bump `Cargo.toml`'s version, commit, push a matching
`v<version>` tag — CI attaches the platform archives to the release, and dev
reinstalls (which wipe `bin/`) self-heal by re-downloading.

### Requirements

- The server distribution is downloaded at runtime from JetBrains. Since 1.0
  (2026-10) it is stable but **requires an IntelliJ IDEA Ultimate license**
  after a 30-day trial (enforced by the launcher, like the EULA).
- `JAVA_HOME` / JDK resolution (used for the server's `JAVA_HOME`, its Maven
  import forks via `JB_MAVEN_JAVA_HOME`, and `defaultSdk`):
  1. `lsp.intellij-server.settings.java_home` per project,
  2. `$JAVA_HOME` from your shell environment.

  When neither is configured the server falls back to its own JVM discovery
  (`/usr/bin/java`).
- `lsp.intellij-server.settings.vm_options` tunes server JVM flags;
  `lsp.intellij-server.settings.proxy_path` overrides the proxy binary.

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
