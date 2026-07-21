# taskshoot-cli

Taskshoot のタスク操作 CLI (Rust)。AI エージェントが「未着手タスクを拾う → 着手 →
開発 → 完了」の定型フローを回すことを主目的に、Web でできる管理系以外のタスク操作を
コマンドラインから行える。

## ビルド / インストール

```bash
cd client-cli
cargo build --release
# バイナリ: target/release/taskshoot
cargo install --path .   # ~/.cargo/bin/taskshoot に入れる場合
```

## 認証設定

API キー (`tssk-...`) は taskshoot の `/settings/api-keys` (user キー)、または
組織管理の Bot users からボットに発行する。write 操作には write 権限付きキーが必要。
組織スコープキー (deprecated) は使えない。

読み込み優先順位:

1. **環境変数直接** — `TASKSHOOT_API_KEY` / `TASKSHOOT_CLI_ORGANIZATION`。
   CI や AI エージェントが直接渡すケース。1Password の Touch ID 承認が使えない
   プロセスではこの経路を使う。
2. **getter command** — `TASKSHOOT_CLI_ENV_GETTER_COMMAND` に設定されたコマンドを
   (シェルを介さず) 実行し、stdout を env-file 形式 (`KEY=VALUE`、`#` コメント可)
   としてパースする。
3. **`.loadenv.sh` 探索** — カレントディレクトリから上位へ、次に実行ファイルの
   ディレクトリから上位へ `.loadenv.sh` を探し、`export TASKSHOOT_CLI_ENV_GETTER_COMMAND=...`
   の行だけを抽出して 2 と同様に実行する (ファイル全体をシェル実行はしない)。
   **発見したファイルは direnv の allow と同様に、`taskshoot trust <path>` で明示的に
   信頼したものだけ実行される** (信頼情報は `~/.config/taskshoot/trusted-loadenv` に
   内容のハッシュ付きで記録され、ファイルが変更されると再信頼が必要)。悪意ある
   リポジトリ配下で CLI を実行しても任意コマンドが走らないようにするための仕組み。

> **`taskshoot trust` が必要なのは経路 3 だけ**。以下の構成 (シェルプロファイルや
> ラッパースクリプト経由の export を含む) では `.loadenv.sh` の探索自体が走らないため、
> `.loadenv.sh` も trust も不要 (`~/.config/taskshoot/trusted-loadenv` が存在しなくてよい)。
> この構成で `taskshoot trust` を引数無しで実行すると「trust は不要」と表示して正常終了する。
>
> - `TASKSHOOT_CLI_ENV_GETTER_COMMAND` を export している (探索は必ずスキップされる)
> - `TASKSHOOT_API_KEY` **と** `TASKSHOOT_CLI_ORGANIZATION` の両方を export している
>
> **`TASKSHOOT_API_KEY` だけでは不十分**な点に注意。org スコープのコマンド
> (`projects` / `tasks` 等) は `--org` も `TASKSHOOT_CLI_ORGANIZATION` も無いと、
> **org を解決する目的で `.loadenv.sh` を探索する** (`resolve()` の
> `need_org && org_unresolved` 分岐)。この場合は未 trust の候補がスキップされ
> `TASKSHOOT_CLI_ORGANIZATION is not set` で失敗するため、trust (または org の指定) が要る。
> `me` / `orgs` (org 不要) はキーだけで動く。

### 1Password でのセットアップ例

1Password の development vault に item を作り、env-file 形式のフィールドを入れる:

```
TASKSHOOT_CLI_ORGANIZATION=cyberneura
TASKSHOOT_API_KEY=tssk-...
```

`.loadenv.sh` (グローバル gitignore 済み) を**探索されるディレクトリ**に作り、信頼登録する。
探索は「カレントの祖先」と「実行ファイルの祖先」のみで、**子ディレクトリは見ない**点に注意:

- **リポジトリ内で使う場合**: リポジトリルートに置く (リポジトリ内のどこから実行しても
  祖先として発見される)。`client-cli/` に置いた場合は `client-cli/` 配下で実行した時だけ見つかる。
- **`cargo install` したバイナリをどこからでも使う場合**: `~/.loadenv.sh` に置く
  (`$HOME` は `~/.cargo/bin` の祖先なので実行ファイル側の探索で見つかる)。
  またはシェルプロファイルで `TASKSHOOT_CLI_ENV_GETTER_COMMAND` を export する (探索不要)。

```sh
echo 'export TASKSHOOT_CLI_ENV_GETTER_COMMAND='"'"'op read "op://development/taskshoot/taskshoot-cli"'"'"' > .loadenv.sh   # リポジトリルート
taskshoot trust .loadenv.sh   # 1回だけ。ファイルを書き換えたら再実行
```

### API 接続先

デフォルトは `https://taskshoot-api.cyberneura.com`。ローカル開発サーバーに向けるには:

```bash
export TASKSHOOT_API_ORIGIN=http://127.0.0.1:8008
```

## 使い方

すべてのコマンドに `--json` (生 JSON 出力) と `--org <code>` (組織上書き) がある。
exit code は成功 0 / エラー 1。

```bash
taskshoot me                                   # 認証確認 (誰として動いているか)
taskshoot orgs                                 # アクセス可能な組織一覧 (org 未設定でも可)
taskshoot projects                             # プロジェクト一覧
taskshoot workflows --project DEV              # 進行フローとステージ一覧 (値/ラベル/terminal)
taskshoot categories --project DEV             # タスクカテゴリー一覧 (id/名前)

taskshoot tasks --project DEV                  # タスク一覧
taskshoot tasks --project DEV --status 起案 --assignee me
taskshoot tasks --project DEV --mentioned me   # 自分に @ メンションがあるタスク
taskshoot tasks --project DEV --mentioned suzuki   # 特定の人宛 (handle / 表示名 / id も可)
taskshoot tasks --project DEV --untracked      # casual タスクのみ
taskshoot tasks --project DEV --bot-ready true # Bot が着手可のタスクのみ

taskshoot search "検索 インデックス"          # 組織横断のタスク検索 (--limit 1-50)
taskshoot search DEV-12                        # KEY-番号 の直接参照もヒットする

taskshoot task show DEV-12
taskshoot task create --project DEV --title "新機能" --description "..." --assignee me
taskshoot task create --project DEV --content "メモ的な相談"   # untracked (番号なし)
taskshoot task update DEV-12 --status 対応中 --progress 50
taskshoot task update DEV-12 --bot-ready true  # Bot 着手可フラグ (変更はログに残る)
taskshoot task update DEV-12 --category 開発   # カテゴリー設定 (名前 or id。"" でクリア)
taskshoot task claim DEV-12                    # assignee=me + 対応中へ (--status で上書き可)
taskshoot task claim DEV-12 --if-unassigned    # 未 assign の時だけ claim (他が取得済みなら 409)
taskshoot task complete DEV-12 --comment "対応完了しました"     # 終端ステージへ
taskshoot task comment DEV-12 "進捗コメント" --file ./screenshot.png
taskshoot task events DEV-12                   # スレッド表示
taskshoot task track <uuid> --project DEV      # untracked → tracked (番号採番)
taskshoot task cancel DEV-12 --reason "重複のため"
taskshoot task resume DEV-12
```

通知 (mention inbox):

```bash
taskshoot notifications list                   # 自分宛の通知一覧 (新しい順) + 未読数
taskshoot notifications list --unread-only     # 未読のみ
taskshoot notifications list --limit 50 --json # AI エージェント向け (最大 100)
taskshoot notifications read <id> [<id> ...]   # 指定 id を既読にする (要 write キー)
taskshoot notifications read --all             # 全通知を既読にする
```

- タスク参照は `KEY-番号` (例 `DEV-12`)。untracked タスクは番号を持たないため
  UUID + `--project` で指定する。
- `--status` はラベル (例 `対応中`) か数値 (例 `40`) のどちらでも指定できる。
  `task update` 等の単一タスク操作ではそのタスクの workflow のステージから解決する。
  `tasks --status` (一覧フィルタ) はプロジェクトの全 workflow から解決し、同一ラベルが
  複数の値に解決される場合はエラーになる (数値指定で回避)。
- `task complete` は workflow の terminal ステージへ変更する。プロジェクトの
  workflow に検収フローがある場合、タスクは完了ではなく検収フェーズに入る (仕様)。
  `--comment` は完了成功後にスレッドへ投稿される。
- `tasks --mentioned <user>` は「そのユーザー宛の @ メンションが description か
  コメントにあるタスク」に絞り込む (サーバー側フィルタ `mentioned_user_id`)。
  ユーザーは `--assignee` と同じく "me" / handle name / 表示名 / user id で指定できる。
  照合はフロントエンドのメンション表示と同じ規則 (handle_name、未設定なら
  メール local part 由来のデフォルト handle) で、所属する MentionGroup 宛
  (@dev-team 等) も含む。
- `task create --status` は作成 API がステータスを初期ステージに戻す仕様のため、
  作成後に PATCH で反映している (見た目は 1 コマンド)。
- `--category` (create / update) はカテゴリー名 (大文字小文字無視) か id で指定する。
  一覧は `taskshoot categories --project <KEY>`。`update --category ""` でクリアできる。
- `me` / `orgs` / `notifications` は組織未設定 (`TASKSHOOT_CLI_ORGANIZATION` なし)
  でも実行できる (通知はユーザースコープで組織横断)。
- `notifications` は自分宛の通知 (メンション / assign / status 変更等) を扱う。
  ボットには通常 `task_mentioned` (= @bot 宛メンション) のみが届くので、
  自律ループはこれを拾って着手判断に使える。`read` (既読化) は書き込みなので
  write 権限キーが必要。既読は通知ごと (per-item) に記録される。
- `search` は組織内の全プロジェクト横断でタスクを検索する (`/task-search/` API)。
  サーバー側は bigram (部分一致) + ベクトル (意味検索) のハイブリッド。タイトル /
  description / コメント本文が対象で、`KEY-番号` や番号のみの入力は直接ヒットする。

### AI エージェントの定型フロー例

```bash
taskshoot tasks --project DEV --bot-ready true --status 起案 --json  # 着手可の未着手を探す
taskshoot task claim DEV-12 --if-unassigned --json     # 拾う (二重処理防止: 他が取得済みなら 409)
taskshoot task comment DEV-12 "着手します" --json
# ... 開発 ...
taskshoot task complete DEV-12 --comment "実装完了。PR: <URL>" --json
```

複数エージェントで自律的に回す運用は `taskshoot-agent-loop` スキル参照。

## 開発

```bash
cargo test          # ユニットテスト (task_ref 分解 / env パース / ステージ解決)
cargo clippy
cargo fmt
```

API の実体は `backend/taskshoot/task/api.py` ほか django-ninja のルーター定義が正。
エンドポイントを足す時は `src/api.rs` に薄いメソッドを追加し、コマンドは
`src/commands.rs` に置く。
