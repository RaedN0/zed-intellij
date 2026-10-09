---
name: land
description: >-
  Land the current thread's changes into this zed-intellij project. Use only when
  the user has explicitly requested landing (for example the Land Changes button
  or a /land invocation). Do not invoke it merely to review, prepare, verify, or
  check changes.
disable-model-invocation: true
metadata:
  delta-action: land
---

# Landing changes in zed-intellij

## Invocation and intent

- Run this only for an explicit landing request.
- That request already carries merge intent. Do not ask again whether to land, and
  do not answer a `/land` (or Land Changes) invocation by saying you are waiting
  for a landing request.
- Proceed through the workflow below without stopping for routine reconfirmation.
  Stop only for a genuine blocker or unresolved scope.

## How this project lands

- There is no source remote, no CI, and no pull-request flow. The repository has a
  single branch, `main`.
- The landing destination is the user's primary checkout at
  `~/Projects/zed-intellij`, reached through the `local` remote, which is
  configured with `receive.denyCurrentBranch=updateInstead`
  (`git config remote.local.url` / `remote.local.receivepack`). Pushing to that
  remote updates the checkout's working tree directly.
- `bin/` is gitignored (root `.gitignore`); the seeded `kotlin-lsp-proxy` and
  `intellij-dap-bridge` binaries are rebuilt and copied into the Zed extension
  work directory by hand and are never part of a landed change.

## Workflow

1. **Re-verify the destination.** Run `git remote -v` and
   `git --no-optional-locks status --short` in the worktree root. Confirm the
   `local` remote still points at `~/Projects/zed-intellij`. If the set of remotes
   has changed, land only to a destination whose role you can confirm; if that is
   unclear, stop and ask.
2. **Verify the change before landing it** (this repo has no CI, so these local
   builds are the only gate, and they must pass on the exact change being landed):
   - Proxy crate: `cargo build --release` run inside `proxy/`
     (its `[profile.release]` lives in `proxy/Cargo.toml`).
   - Extension: `cargo check --target wasm32-wasip1` run at the repository root
     (root `Cargo.toml` is a `cdylib` built against `zed_extension_api`, and
     `extension.toml` declares this as a Zed extension).
   If the `wasm32-wasip1` target is missing, add it with
   `rustup target add wasm32-wasip1` and retry. If either build fails, stop;
   report that the change did not land and show the failure.
3. **Commit the change.** Stage and commit any uncommitted work in the worktree on
   the current branch with a descriptive message. Include only files that belong
   to the requested change; leave gitignored artifacts alone. If nothing is
   changed and no new commit is needed, say so and skip to the landing check.
4. **Land it.** Fast-forward the branch into the primary checkout:

   ```sh
   git push local main:main
   ```

   This is fast-forward-only. Before pushing, confirm the primary checkout is
   clean (`git -C ~/Projects/zed-intellij --no-optional-locks status --short` is
   empty). If it is dirty, or the push is rejected as non-fast-forward, stop —
   see Conflicts. Never force-push and never rewrite history.
5. **Confirm the result at the destination.** Verify the primary checkout now
   contains the landed commit and is clean:

   ```sh
   git -C ~/Projects/zed-intellij log --oneline -1
   git -C ~/Projects/zed-intellij --no-optional-locks status --short
   ```

   Only report success once the destination's `main` actually contains the change.

## Conflicts

On any conflict (a dirty destination, a rejected non-fast-forward push, or a
conflicting merge), stop and do not guess:

- Report the conflicting paths and what blocks the landing.
- Offer the user the choice: resolve it themselves, or have you resolve it.
- If they choose to resolve it themselves, wait for them and then re-run the
  landing check.
- If they choose to have you resolve it, resolve only when the intended result is
  clear; if it is not, pause and ask. A conflict is never resolved by
  force-pushing or discarding unrelated work.

## Reporting

Finish by stating: whether the change landed, the destination commit it landed as
(`git -C ~/Projects/zed-intellij log --oneline -1`), the verification commands
that passed, and anything that still needs the user (for example re-seeding a
rebuilt proxy binary into the extension work directory, which is not part of
landing). If it did not land, say so plainly and give the blocker.
