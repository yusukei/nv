---
name: wrangler-compose
description: Build, deploy, tear down, back up and restore Cloudflare Workers environments with wrangler-compose. Use whenever a task deploys a Worker, applies D1 migrations to a deployed environment, or removes, backs up or restores a Cloudflare environment.
---

# Cloudflare environments through wrangler-compose

Projects with a Wrangler config (`wrangler.jsonc`, `wrangler.json` or `wrangler.toml`) are deployed with
`wrangler-compose`, not with `wrangler deploy` or `wrangler d1 migrations apply` directly.
The environment name is the one in the Wrangler config (`env.<name>`).

| To do this | Type |
|---|---|
| Check that the Worker builds, without uploading | `wrangler-compose --env <env> build` |
| Deploy (apply pending D1 migrations, then deploy; first time and updates alike) | `wrangler-compose --env <env> up` |
| Back up D1 and R2 to `backups/` | `wrangler-compose --env <env> backup` |

- `down`, `down --purge` and `restore --drop` delete things and ask the user to retype the environment name.
  They refuse to run without a terminal, so do not try to pipe an answer in. Ask the user to run them.
- Secrets come from `.env.<env>` in the project. Never print, commit or copy their values.
- For any other Wrangler command, use `nv run wrangler …` so the project's locked Wrangler version is used.
- Deploying is visible to others. Confirm with the user before running `up` against an environment they did not name.
