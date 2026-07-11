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

taskshoot tasks --project DEV                  # タスク一覧
taskshoot tasks --project DEV --status 起案 --assignee me
taskshoot tasks --project DEV --untracked      # casual タスクのみ

taskshoot task show DEV-12
taskshoot task create --project DEV --title "新機能" --description "..." --assignee me
taskshoot task create --project DEV --content "メモ的な相談"   # untracked (番号なし)
taskshoot task update DEV-12 --status 対応中 --progress 50
taskshoot task claim DEV-12                    # assignee=me + 対応中へ (--status で上書き可)
taskshoot task complete DEV-12 --comment "対応完了しました"     # 終端ステージへ
taskshoot task comment DEV-12 "進捗コメント" --file ./screenshot.png
taskshoot task events DEV-12                   # スレッド表示
taskshoot task track <uuid> --project DEV      # untracked → tracked (番号採番)
taskshoot task cancel DEV-12 --reason "重複のため"
taskshoot task resume DEV-12
```

- タスク参照は `KEY-番号` (例 `DEV-12`)。untracked タスクは番号を持たないため
  UUID + `--project` で指定する。
- `--status` はラベル (例 `対応中`) か数値 (例 `40`) のどちらでも指定できる。
  ラベルはタスクの workflow のステージから解決する。
- `task complete` は workflow の terminal ステージへ変更する。プロジェクトの
  workflow に検収フローがある場合、タスクは完了ではなく検収フェーズに入る (仕様)。
  `--comment` は完了成功後にスレッドへ投稿される。
- `task create --status` は作成 API がステータスを初期ステージに戻す仕様のため、
  作成後に PATCH で反映している (見た目は 1 コマンド)。
- `me` / `orgs` は組織未設定 (`TASKSHOOT_CLI_ORGANIZATION` なし) でも実行できる。

### AI エージェントの定型フロー例

```bash
taskshoot tasks --project DEV --status 起案 --json     # 未着手を探す
taskshoot task claim DEV-12 --json                     # 拾う
taskshoot task comment DEV-12 "着手します" --json
# ... 開発 ...
taskshoot task complete DEV-12 --comment "実装完了。PR: <URL>" --json
```

## 開発

```bash
cargo test          # ユニットテスト (task_ref 分解 / env パース / ステージ解決)
cargo clippy
cargo fmt
```

API の実体は `backend/taskshoot/task/api.py` ほか django-ninja のルーター定義が正。
エンドポイントを足す時は `src/api.rs` に薄いメソッドを追加し、コマンドは
`src/commands.rs` に置く。
