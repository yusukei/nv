# wrangler-compose

2 つのコマンドを、1 つのリポジトリで別々に作る。

| コマンド | 受け持つもの | 持たないもの | 要求仕様 |
|---|---|---|---|
| `nv` | Node・npm を uv のように扱う(Node の版の固定と取得、`sync`・`run`・`x`、スキル、`self update`) | Cloudflare に触れること・Wrangler を動かすこと | [docs/nv](docs/nv/requirements.html) |
| `wrangler-compose` | Wrangler を docker compose のように扱う(`--env <環境名> up / build / down [--purge] / backup / restore`) | Node・npm の取得と版のこと | [docs/wrangler-compose](docs/wrangler-compose/requirements.html) |

## 2 つの境目

- wrangler-compose は、Wrangler を必ず `nv run wrangler …` で動かす。Wrangler の版はプロジェクトの `package-lock.json`、Node の版はプロジェクトの `.node-version` が決める
- nv は Wrangler を使わないプロジェクトでも単独で使える
- 更新(`nv self update`)とエージェント用のスキル(`nv install skill`)は nv が 2 つ分まとめて受け持つ

## 進め方

仕様書は HTML で、ブラウザで [docs/index.html](docs/index.html) を開いて読む。要求仕様と受け入れ条件(`docs/<コマンド>/requirements.html`・`acceptance.html`)を決めてから、設計とコードに入る。nv から作る(wrangler-compose は nv に頼る)。
