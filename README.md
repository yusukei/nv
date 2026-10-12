# nv

Node・npm を uv のように扱う `nv` と、Cloudflare Workers の環境を docker compose のように扱う `wrangler-compose`。2 つのコマンドを、1 つのリポジトリで別々に作る。

| コマンド | 受け持つもの | 持たないもの | 要求仕様 |
|---|---|---|---|
| `nv` | Node・npm を uv のように扱う(Node の版の固定と取得、`sync`・`run`・`x`、スキル、`self update`) | Cloudflare に触れること・Wrangler を動かすこと | [docs/nv](docs/nv/requirements.html) |
| `wrangler-compose` | 手元で開発した Workers のサービス群を docker compose のように動かし、リモートへ展開する(`--env <環境名> up / build / down [--purge] / ps / logs / backup / restore`) | Node・npm の取得と版のこと | [docs/wrangler-compose](docs/wrangler-compose/requirements.html) |

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
nv node list --remote       # 入れられる Node の版(系列ごとの最新・LTS の名前)
echo 24.11.1 > .node-version
nv add semver               # 依存を足す(package-lock.json が無ければ作る)
nv add --dev typescript     # 開発用の依存
nv remove semver            # 依存を外す
nv sync                     # package-lock.json のとおりに入れる
nv run npm run build        # 揃えた Node・npm で動かす
nv run wrangler deploy      # プロジェクトの依存の道具
nv x cowsay@1.6.0 hello     # プロジェクトに入れずに 1 回動かす
nv install skill            # Claude Code と Codex にスキルを入れる
```

nv のものはすべて `~/.nv` の下に置く(Node・npm のキャッシュ・`nv x` の道具は `~/.nv/cache`)。

## wrangler-compose の使い方

nv と同じフォルダに置く(上の「導入」)。Wrangler はプロジェクトの依存に入れておく(`nv run npm install --save-dev wrangler@<版>`、4.149.0 以上)。

プロジェクトの一番上に `wrangler-compose.yaml` を 1 つ置き(展開用のフォルダに分けるなら、そこに置いて `context: ../app` のように Wrangler の入った `package.json` のフォルダを書く)、docker compose と同じ形でサービス(`type: worker / d1 / r2 / kv / queue`)を並べる。環境ごとの違いは `${変数}` だけで書き、値は環境変数か `.env.<環境名>`(git に入れない)から入る。例は [tests/fixture](crates/wrangler-compose/tests/fixture/wrangler-compose.yaml)。

```bash
wrangler-compose --env local up           # 手元で全サービスを起動(ソースを直すと組み直す。Ctrl-C で止める)
wrangler-compose --env local ps           # 動いているか・URL・未適用の D1 移行
wrangler-compose --env local logs -f api  # サービス名つきのログ
wrangler-compose --env local backup       # D1・R2・KV を backups/local/<日時>/ へ
wrangler-compose --env local restore --drop backups/local/<日時>
wrangler-compose --env local down         # 止める(データは残る)
wrangler-compose --env local down --purge # データも消す
wrangler-compose --env local --dry-run up # 何も変えずに、何をするかを示す(どの操作にも付く)
```

`--env local` 以外の名前(`staging`・`pr-123` など)はリモートの環境で、Cloudflare に `<プロジェクト>-<環境>-<サービス>` などの名前で建てる。要るのは一度の `nv run wrangler login` と、`.env.<環境名>` の `CLOUDFLARE_ACCOUNT_ID=<アカウントの ID>`(必須。意図しないアカウントへ出さない・消さないため。環境変数の値が違えば止まる)。

```bash
wrangler-compose --env staging --dry-run up   # 何を作り・上げるかを示す
wrangler-compose --env staging up             # 資源を作り、移行を当て、Worker を上げる(初めての環境は打ち直しで確かめる)
wrangler-compose --env staging ps
wrangler-compose --env staging logs api
wrangler-compose --env staging backup         # 控えは環境によらず同じ形。手元へ restore もできる
wrangler-compose --env staging down
```

リモートは本物の Cloudflare(無料プラン)で `up`・`ps`・`logs`・`backup`・`down --purge` を確かめた。ただし新しく足した cron が Cloudflare 側で発火しないことがあり(素の `wrangler deploy` でも同じ)、cron は無料プランでアカウントに 5 つまで。手元で動かないものは [docs/wrangler-compose/local-support.html](docs/wrangler-compose/local-support.html) にある。

## 進め方

仕様書は HTML で、ブラウザで [docs/index.html](docs/index.html) を開いて読む。要求仕様と受け入れ条件(`docs/<コマンド>/requirements.html`・`acceptance.html`)を決めてから、設計とコードに入る。nv から作る(wrangler-compose は nv に頼る)。
