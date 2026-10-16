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

- `kotlin-lsp-proxy` (from `proxy/`) rewrites the archive-internal
  (`jar!/`, `zip!/`) source URIs the server returns into real files so
  Go-to-Definition into library/JDK sources works. Optional — without it the
  server still runs.
- `intellij-dap-bridge` (from `bridge/`) is the debug adapter. Required for
  debugging (Unix sockets; no Windows).

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
  2. `$JAVA_HOME` from your shell environment,
  3. fallback: the author's sdkman path (`DEFAULT_JAVA_HOME` in `src/lib.rs`) —
     on machines where it doesn't exist, configure 1 or 2 or the server's
     Maven forks fall back to `/usr/bin/java`.
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
