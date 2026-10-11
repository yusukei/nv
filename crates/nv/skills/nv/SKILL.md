---
name: nv
description: Run Node.js, npm, npx and JavaScript tools through nv. Use whenever a task needs node, npm, npx, a package.json script, or any JavaScript file or CLI from node_modules.
---

# Node.js through nv

This machine manages Node.js with `nv`. Never type `node`, `npm` or `npx` directly:
the versions found on PATH differ between machines, and on Windows they are often missing.

| To do this | Type |
|---|---|
| Install the project's dependencies exactly as locked | `nv sync` |
| Run a JavaScript file | `nv run node <script.js> [args…]` |
| Run a package.json script | `nv run npm run <script>` |
| Run a tool installed in the project (e.g. wrangler, vite, tsc) | `nv run <tool> [args…]` |
| Add or change a dependency | `nv run npm install <package>@<version>` |
| Run a tool that is not in the project, once | `nv x <package>@<exact version> [args…]` |

- `nv run` installs the pinned Node and the locked dependencies first when they are missing.
- The Node version comes from `.node-version` next to `package.json` and must be exact (e.g. `24.11.1`).
  If nv stops because it is missing, ask the user which version to pin; do not guess.
- `nv x` needs an exact version (`cowsay@1.6.0`, not `cowsay` or `cowsay@latest`).
  Outside a project, pass the Node version with `--node <version>`.
- nv's own messages go to stderr; stdout belongs to the command it runs.
