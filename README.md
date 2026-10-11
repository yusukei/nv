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

## nv の導入

Release から OS に合う実行ファイルを取り、1 つのフォルダに置く(インストーラも PATH の書き換えも要らない。PATH に入れるかは自分で決める)。`<owner>/<repo>` はこのリポジトリ。

macOS(Apple Silicon)。ブラウザで取ると隔離の印が付いて止められるので、`curl` で取る:

```bash
mkdir -p ~/.local/bin && curl -fL -o ~/.local/bin/nv https://github.com/<owner>/<repo>/releases/latest/download/nv-aarch64-apple-darwin && chmod +x ~/.local/bin/nv
```

Windows(PowerShell):

```powershell
New-Item -ItemType Directory -Force "$HOME\.local\bin" | Out-Null; Invoke-WebRequest -OutFile "$HOME\.local\bin\nv.exe" https://github.com/<owner>/<repo>/releases/latest/download/nv-x86_64-pc-windows-msvc.exe
```

ほかの OS は `nv-x86_64-apple-darwin`・`nv-x86_64-unknown-linux-musl`。以後の更新は `nv self update`。

## nv の使い方

```bash
echo 24.11.1 > .node-version
nv run npm install          # 最初の package-lock.json を作る
nv sync                     # package-lock.json のとおりに入れる
nv run npm run build        # 揃えた Node・npm で動かす
nv run wrangler deploy      # プロジェクトの依存の道具
nv x cowsay@1.6.0 hello     # プロジェクトに入れずに 1 回動かす
nv install skill            # Claude Code と Codex にスキルを入れる
```

nv のものはすべて `~/.nv` の下に置く(Node・npm のキャッシュ・`nv x` の道具は `~/.nv/cache`)。

## 進め方

仕様書は HTML で、ブラウザで [docs/index.html](docs/index.html) を開いて読む。要求仕様と受け入れ条件(`docs/<コマンド>/requirements.html`・`acceptance.html`)を決めてから、設計とコードに入る。nv から作る(wrangler-compose は nv に頼る)。
