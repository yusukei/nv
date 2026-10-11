# nv

Node・npm を uv のように扱う `nv` と、Cloudflare Workers の環境を docker compose のように扱う `wrangler-compose`。2 つのコマンドを、1 つのリポジトリで別々に作る。

| コマンド | 受け持つもの | 持たないもの | 要求仕様 |
|---|---|---|---|
| `nv` | Node・npm を uv のように扱う(Node の版の固定と取得、`sync`・`run`・`x`、スキル、`self update`) | Cloudflare に触れること・Wrangler を動かすこと | [docs/nv](docs/nv/requirements.html) |
| `wrangler-compose` | Wrangler を docker compose のように扱う(`--env <環境名> up / build / down [--purge] / backup / restore`) | Node・npm の取得と版のこと | [docs/wrangler-compose](docs/wrangler-compose/requirements.html) |

## 2 つの境目

- wrangler-compose は、Wrangler を必ず `nv run wrangler …` で動かす。Wrangler の版はプロジェクトの `package-lock.json`、Node の版はプロジェクトの `.node-version` が決める
- nv は Wrangler を使わないプロジェクトでも単独で使える
- 更新(`nv self update`)とエージェント用のスキル(`nv install skill`)は nv が 2 つ分まとめて受け持つ

## 導入

Release から OS に合う `nv` と `wrangler-compose` を取り、同じフォルダ(下の例では `~/.local/bin`)に置く。インストーラも PATH の書き換えも要らない(PATH に入れるかは自分で決める)。wrangler-compose を使わないなら `nv` だけでよい。

macOS はブラウザで取ると隔離の印が付いて止められるので、どの OS もコマンドで取る。

### macOS(Apple Silicon)

```bash
R=https://github.com/yusukei/nv/releases/latest/download; mkdir -p ~/.local/bin && for b in nv wrangler-compose; do curl -fL -o ~/.local/bin/$b "$R/$b-aarch64-apple-darwin" && chmod +x ~/.local/bin/$b; done
```

### macOS(Intel)

```bash
R=https://github.com/yusukei/nv/releases/latest/download; mkdir -p ~/.local/bin && for b in nv wrangler-compose; do curl -fL -o ~/.local/bin/$b "$R/$b-x86_64-apple-darwin" && chmod +x ~/.local/bin/$b; done
```

### Linux(x64)

```bash
R=https://github.com/yusukei/nv/releases/latest/download; mkdir -p ~/.local/bin && for b in nv wrangler-compose; do curl -fL -o ~/.local/bin/$b "$R/$b-x86_64-unknown-linux-musl" && chmod +x ~/.local/bin/$b; done
```

### Windows(x64、PowerShell)

```powershell
$R = "https://github.com/yusukei/nv/releases/latest/download"; $D = "$HOME\.local\bin"; New-Item -ItemType Directory -Force $D | Out-Null; foreach ($b in "nv", "wrangler-compose") { Invoke-WebRequest -OutFile "$D\$b.exe" "$R/$b-x86_64-pc-windows-msvc.exe" }
```

### 入ったかを確かめる

```bash
~/.local/bin/nv --version
```

Windows は `& "$HOME\.local\bin\nv.exe" --version`。以後の更新は `nv self update`(同じフォルダの wrangler-compose も同じ版になる)。

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

## wrangler-compose の使い方

nv と同じフォルダに置く(上の「導入」)。Wrangler はプロジェクトの依存に入れておく(`nv run npm install --save-dev wrangler@<版>`)。

```bash
wrangler-compose --env staging --dry-run up   # 何も変えずに、何をするかを示す(どの操作にも付く)
wrangler-compose --env staging build      # 組むだけ(何も上げない)
wrangler-compose --env staging up         # D1 の移行 → デプロイ(初回も更新も)
wrangler-compose --env staging backup     # D1・R2 を backups/staging/<日時>/ へ
wrangler-compose --env staging restore --drop backups/staging/<日時>
wrangler-compose --env staging down       # Worker だけ消す(データは残る)
wrangler-compose --env staging down --purge
```

シークレットは `.env.<環境名>` に書く(git に入れない)。R2 を扱う `--purge`・`backup`・`restore` には、同じファイルに `WRANGLER_COMPOSE_R2_ACCESS_KEY_ID`・`WRANGLER_COMPOSE_R2_SECRET_ACCESS_KEY` が要る。

## 進め方

仕様書は HTML で、ブラウザで [docs/index.html](docs/index.html) を開いて読む。要求仕様と受け入れ条件(`docs/<コマンド>/requirements.html`・`acceptance.html`)を決めてから、設計とコードに入る。nv から作る(wrangler-compose は nv に頼る)。
