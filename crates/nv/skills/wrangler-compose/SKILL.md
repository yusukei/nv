---
name: wrangler-compose
description: Run, inspect, stop, back up and restore Cloudflare Workers projects described by wrangler-compose.yaml. Use whenever a project has a wrangler-compose.yaml and a task starts its services locally, reads their logs, applies D1 migrations, or removes, backs up or restores an environment.
---

# Cloudflare services through wrangler-compose

Projects with a `wrangler-compose.yaml` are run with `wrangler-compose`, not with `wrangler dev`,
`wrangler deploy` or `wrangler d1 migrations apply` directly. The file lists services like docker compose
(`type: worker`, `d1`, `r2`, `kv`, `queue`); differences between environments come only from `${VAR}`
values in the environment or `.env.<env>`. `--env local` runs everything on this machine; any other name is a
remote environment on Cloudflare (needs `nv run wrangler login` once).

| To do this | Type |
|---|---|
| Start every service locally (rebuilds on source changes, Ctrl-C stops) | `wrangler-compose --env local up` |
| Check that the Workers build | `wrangler-compose --env local build` |
| See what runs, its URLs and pending D1 migrations | `wrangler-compose --env local ps` |
| Read logs tagged with the service name | `wrangler-compose --env local logs [-f] [service…]` |
| Stop the local services (data stays) | `wrangler-compose --env local down` |
| Back up D1, R2 and KV to `backups/` | `wrangler-compose --env local backup` |
| See what any command would do, changing nothing | add `--dry-run`, e.g. `wrangler-compose --env local --dry-run up` |

- `up` stays in the foreground; run it in the background when you need the terminal, and stop it with `down`.
- For a remote environment, run `--dry-run up` first and show the user what it would create and deploy.
  The first `up` of a new environment, `--remove-orphans` and dropping a Durable Object class ask the user to
  retype the environment name; deploying is visible to others, so confirm before `up` to an environment the
  user did not name.
- `down --purge` and `restore --drop` delete data and ask the user to retype the environment name.
  They refuse to run without a terminal, so do not try to pipe an answer in. You may run them with
  `--dry-run` to show what would be deleted; ask the user to run the real command.
- Behaviour settings (bindings, vars, routes, crons) belong in `wrangler-compose.yaml`, not in a
  Worker folder's Wrangler config; that config may only hold build settings.
- A Worker's non-secret values go in its `environment` and its secrets in its `secrets`, both as in docker
  compose (`NAME: value` or a list). A secret's value must be `${VAR}`; values come from `.env.<env>`.
  Never print, commit or copy secret values.
- `${WC_R2_<SERVICE>_S3}` in a Worker's `environment` is the bucket's S3-compatible endpoint, filled in by
  the tool for local and remote; do not write endpoints per environment by hand.
- A remote environment needs `CLOUDFLARE_ACCOUNT_ID=<account id>` in its `.env.<env>`; every remote command,
  even `--dry-run`, stops without it or when the environment variable differs. Ask the user for the ID;
  do not pick an account yourself.
- `context: <folder>` at the top points at the folder whose `package.json` holds Wrangler, when the compose
  file lives elsewhere (e.g. a `deploy/` folder).
- For any other Wrangler command, use `nv run wrangler …` so the project's locked Wrangler version is used.
