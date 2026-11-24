# Configuration

> Status: P1 最小実装 + P2 拡張 (indexing / search / launcher 実装、keybindings / plugins 予約)
> Related: [`ROADMAP.md`](./ROADMAP.md), [`docs/persistence.md`](./persistence.md)

本書はユーザー設定ファイル `config.toml` の配置・スキーマ・ロード戦略・hot reload を定めます。

---

## 1. ファイル配置 (XDG 準拠)

| データ種別 | 場所 |
|-----------|------|
| **config** | `$XDG_CONFIG_HOME/nohrs/config.toml` (Linux/macOS: `~/.config/nohrs/config.toml`) |
| **SQLite DB** (メタデータ・履歴) | `$XDG_DATA_HOME/nohrs/db.sqlite` |
| **redb (ホスト KV, P2)** | `$XDG_DATA_HOME/nohrs/state.redb` |
| **Tantivy index** | `$XDG_DATA_HOME/nohrs/index/` |
| **redb (plugin KV, P4)** | `$XDG_DATA_HOME/nohrs/plugin-kv.redb` |
| **ログ / キャッシュ** | `$XDG_CACHE_HOME/nohrs/` |
| **plugin インストール先** | `$XDG_DATA_HOME/nohrs/plugins/<plugin-id>/` |
| **window position・タブ/セッション復元など高頻度更新** | redb `state.redb` (config.toml ではない、詳細は [`docs/persistence.md`](./persistence.md)) |

XDG 環境変数 `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_CACHE_HOME` を直接解決し、未設定時は `~/.config` / `~/.local/share` / `~/.cache` にフォールバックして上記を組み立てる。`dirs::config_dir()` / `data_dir()` / `cache_dir()` は **使わない**: macOS では `~/Library/Application Support` 等の OS ネイティブパスを返してしまい、本プロジェクトが全プラットフォームで意図する XDG スタイルの dotfile パス (`~/.config` など) と矛盾するため。

---

### Configuration file groups

The main `config.toml` keeps general, explorer, and layout settings. Appearance and color settings are stored in `appearance.toml`; indexing and search settings are stored in `search.toml`. Missing split files are normal. At load time, values in the split files override matching categories from `config.toml`, so existing installations remain compatible.

Settings > About > Storage can enable portable mode on Windows. The portable profile is also detected from `userdata/config.toml` when moved to a computer without the original system config. With it enabled, all application-owned files are under `userdata` beside the executable: configuration files at the root, persistent databases in `data`, Everything index/cache in `cache`, batch-rename session state in `state`, rolling/panic logs in `logs`, and IPC endpoint files in `runtime`. On first launch after switching profiles, missing files are copied from the previous profile before stores are opened; existing destination files are never overwritten. What is copied is the configuration files, `data` and `state` except `state/logs`; the `cache` (the Everything index and the extracted quick-launch icons) and the log history are not, since both are rebuilt from nothing in the new profile, and only `panic.log` is carried over as the crash record worth keeping. This does not relocate user files, Windows Quick Access data, or third-party application settings such as Listary preferences.
## 2. P1 最小スキーマ

```toml
schema_version = 1

[theme]
mode = "dark"   # "light" | "dark" | "system"
accent = "orange"   # "orange" | "blue" | "teal" | "green" | "purple" | "pink" | "red" | hex

[ui]
app_name = ""                # 窗口标题；留空时显示 Prime Files
default_sort = "name"      # "name" | "modified" | "size" | "kind"
default_sort_descending = false  # true = Z→A, maior/mais novo primeiro
show_hidden = false
icon_pack = "default"
file_type_colors = [
  { pattern = "*.webp;*.svg", color = "#ff8800" },
  { pattern = "README*", color = "#22aaff" },
]

[keybindings]
# 草案 (P3 で本格化)。`action = "chord"` の自由マップ。空ならデフォルトは built-in。
# 任意のアクション名を forward-compat のため warn なしで受理 (値は string のみ)。
# quit = "ctrl-q"

[plugins]
# 型予約のみ (P4 で本格運用)。リストは parse/格納されるが host はまだロードしない。
# core = ["git", "calculator"]
# community = ["user/repo", "https://..."]
```

`[keybindings]` と `[plugins]` は型として定義済みだが、値はまだ挙動に反映されない (hot reload は再起動扱い、§5)。スキーマには予約済みで、P3/P4 で形を拡張しても古いファイルは壊れない。

> **ランタイム反映状況**: `theme` / `ui` / `diagnostics` / `indexing.source` はランタイムに反映される。`ui.file_type_colors` の `pattern` はセミコロンで区切った複数の、大文字と小文字を区別しないファイル名 glob に対応する (例: `*.webp;*.svg`)。上から最初に一致した色を一覧・グリッド・コンパクト表示のファイル名に適用する。`indexing.mode` / `indexing.exclude` / `search` / `launcher` / `keybindings` / `plugins` は **parse + validate されるが、まだ挙動には反映されない** (各サブシステムが配線される後続フェーズで有効化)。schema / template に先行して載せているのは、ファイルとエディタ補完が形を先取りできるようにするため。編集しても今は効果がない点に注意。
>
> **`required` なし**: 全フィールドが `#[serde(default)]`。ローダは `Config::default()` から開始して存在するキーだけ上書きするため、どのセクションを省略しても (空の `config.toml` でも) 受理される。生成スキーマも `required` を持たず、エディタ検証とローダ挙動が一致する。

### P2 拡張 (schema 定義済み / 一部はランタイム反映済み)

```toml
[indexing]
# コンピューター全体の名前検索 (Ctrl+E) が名前をどこから取るか:
#   "auto"        Everything が起動中ならそのインデックス、なければ自前 (最初の検索時に決定)
#   "prime-files" 自前のインデックス (ドライブ走査、または NTFS MFT を読むサービス)
#   "everything"  Everything のインデックスだけを使う (SDK DLL 経由、ここでは索引を作らない)
source = "auto"
mode = "auto"  # "auto" | "always-on" | "manual" (予約)

[indexing.exclude]
paths = []
globs = []

[search]
backend = "auto"  # "tantivy" | "ripgrep" | "auto"

[launcher]
hotkey = "Cmd+Shift+Space"
position_remember = true

[diagnostics]
runtime_log = false       # GUI の実行ログをファイルに書く (再起動が必要)
log_max_file_mb = 50      # 1 日ぶんのファイルはここで止める (MB)
log_max_total_mb = 200    # 起動時にこの合計を超える古いファイルから消す (MB)

[diagnostics.store]
# パフォーマンス解析用のストア操作ログ。デフォルト全 off (本番では無音)。
# 出力は tracing 経由 (target = "nohrs_store::sql" / "nohrs_store::redb")、RUST_LOG で絞り込み可。
# 詳細は docs/persistence.md §5。
log_all_queries = false   # 全 SQL クエリを debug で記録 (verbose)
slow_query_ms   = 0       # >0 のとき、この閾値(ms)超過の SQL クエリを warn で記録 (0 = 無効)
log_redb_ops    = false   # redb の get/put/delete/batch 操作と所要時間を記録
```

### 空间热力图 ([space_map])

```toml
[space_map]
# 方块颜色的依据: "size" 按体积 (热力图), "kind" 按文件类型
color = "size"

# 三档体积阈值 (GB, 由大到小): ≥10GB 用第 1 种颜色, 5–10GB 第 2 种,
# 1–5GB 第 3 种, 不足 1GB 第 4 种
bands_gb = [10, 5, 1]

# 四种 #rrggbb 颜色, 与上面的四档一一对应 (默认 红/橙/浅橙/绿)
colors = ["#ef4444", "#f97316", "#fb923c", "#22c55e"]
```

设置页的「空间热力图」分组可以改颜色依据、阈值和四档颜色; 写入的就是上面三项。`color = "kind"` 时按文件夹/图片/视频/音频/文档/压缩包/程序着色, 与大小无关。阈值必须是三个递减的正整数, 颜色必须是四个 `#rrggbb`; 不合法时保留默认值并给出诊断。

### クイック起動 (`quicklaunch.toml`)

クイック起動の設定は `config.toml` のセクションではなく、**独立した `quicklaunch.toml`** に置く。
UI から編集するとファイル全体を書き直すため、`config.toml` の手書きコメントを巻き込まないように
分けている。場所は有効な設定ディレクトリの直下
(`paths::quick_launch_file()`)。XDG なら `$XDG_CONFIG_HOME/nohrs/quicklaunch.toml`
(未設定時は `~/.config/nohrs/quicklaunch.toml`)、ポータブルモードなら
`userdata/quicklaunch.toml`。抽出したアイコンのキャッシュは
`cache/quick_launch_icons` (ポータブルモードでは `userdata/cache/quick_launch_icons`) に置く。

```toml
schema_version = 1

[window]
hotkey = "ctrl+alt+Space"   # global-hotkey の形式。空文字なら OS に登録しない
view = "grid"               # "grid" | "list"
columns = 8                 # 2..=16 (旧バージョンとの互換。今は cell_size と窓の幅で決まる)
cell_size = 100             # 64..=160。格子はこの大きさを目安に窓を埋める
icon_size = 64              # 16/24/32/48/64 のどれか (他の値は最も近いもの)
show_cells = true           # 格子の枠を描く (空きマスも)
list_columns = 2            # 1..=3 (旧バージョンとの互換。今は窓の幅で決まる)
auto_hide = true
topmost = false
close_after_launch = true
on_running = "activate"     # "activate" | "new" | "ask"
keep_running = true

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"                # app | folder | file | url | uwp | command | group | plugin:<id>
name = "Editor"
path = "%ProgramFiles%\\Notepad++\\notepad++.exe"
args = ""
```

- `[window]` は省略可能で、欠けたキーは既定値になる。`[[group]]` の `id` と `name`、
  `[[group.item]]` の `slot` / `type` / `name` は必須。項目の任意フィールドは
  `id` / `path` / `args` / `cwd` / `admin` / `icon` / `tags` / `items` / `extra`。
- ファイルが無い場合は `general` という空のグループ 1 つを持つ既定設定をメモリ上でだけ使い、
  最初の編集までファイルを作らない。
- 構文・型・未知フィールドのエラーと、このビルドより新しい `schema_version` は
  **読み取り専用**として扱う。画面に理由を出し、UI からの保存を拒否する
  (新しいビルドのフィールドを消してしまわないため)。
- 数値の範囲外は丸め、`id` の不正・名前の空・スロットや id の重複は該当項目を捨てて警告にする。
  警告はログに出し、起動は止めない。UTF-8 BOM 付きのファイルも読める
  (PowerShell 5.1 や旧版のメモ帳が書き込むため)。
- 書き込みは 300 ms にまとめ、一時ファイル → `fsync` → `.bak` → rename の順で行う。
  書き直すため、手書きのコメントと整形は失われる。
- 変更は監視されており、外部で編集された場合も内容のハッシュが変わった時点で読み直す。

#### グループの `source` (動的分組)

`[[group]]` の `source` は省略できる。書くとそのグループの項目はファイルではなく実行時に
生成され、ファイルには項目を書かない (書いても破棄され、警告が出る)。

```toml
[[group]]
id = "tools"
name = "Ferramentas"
source = "folder:%USERPROFILE%\\bin"   # folder:<パス> / "recent" / "most_used"
```

- `folder:<パス>` はそのディレクトリの直下だけ (再帰しない) を、名前の大文字小文字を無視して
  並べる。隠しファイルとシステムファイルは除き、スロット 0 から最大 512 件。読み直しは
  30 秒に 1 回までで、ファイル監視はしない。ディレクトリが無いときは項目が空になる。
- `recent` は最終使用の新しい順、`most_used` は回数の多い順 (同数は最終使用順)。
  現在も設定に残っている元項目だけを対象にし、最大 `columns × 2` (32 件まで)。
- 常駐モード (`state.redb` を開かない) では `recent` / `most_used` は使えない。
- 動的分組の項目は読み取り専用。`noh ql list` は `(dynamic: <source>)` とだけ表示し、
  `noh ql add` / `remove` / `run` は拒否する。
- それ以外の `source` は警告のうえ「不明」として扱い、書かれた項目を持つ普通のグループになる。
- `source` を書いたグループは、保存時に `id` / `name` / `source` だけが書き戻される。

#### `command` の `extra`

`type = "command"` の項目は `path` の代わりに `extra` でシェルスクリプトを指定する。
`path` と `args` は使わない。

```toml
[[group.item]]
slot = 1
type = "command"
name = "Build"
cwd = "%USERPROFILE%\\src"

[group.item.extra]
shell = "pwsh"             # 必須: "cmd" | "pwsh" | "powershell" | "python"
script = "npm run build"   # 必須: 空でない文字列
hidden = false             # 省略可: true で CREATE_NO_WINDOW (ウィンドウを出さない)
keep_open = false          # 省略可: true で終了後もコンソールを開いたまま
```

| キー | 必須 | 既定 | 意味 |
|---|---|---|---|
| `shell` | はい | — | 実行するシェル。`cmd` / `pwsh` (PowerShell 7) / `powershell` (Windows PowerShell) / `python` のいずれか |
| `script` | はい | — | シェルに渡すスクリプト。空文字は不可 |
| `hidden` | いいえ | `false` | `true` なら `CREATE_NO_WINDOW` でウィンドウを出さない |
| `keep_open` | いいえ | `false` | `true` なら終了後もコンソールを開いたままにする (`cmd` は `/k`、PowerShell は `-NoExit`、Python は `-i`) |

- `shell` が上の 4 つ以外、`script` が空、`hidden` / `keep_open` が真偽値でないときは、
  その項目は起動できない (健全性チェックが「無効」を返す)。`extra` の未知のキーは警告だけで、
  項目は残る。
- `admin = true` の `command` はこの期では未対応 (D4)。
- 初回の起動時は確認の浮層が出る。信頼の記録は `shell` / `script` / 書かれた `cwd` /
  `hidden` / `keep_open` のハッシュで、設定ページのフォームから保存したときだけ自動で信頼される。
- `cwd` を省略すると `%USERPROFILE%`。存在しないディレクトリを指すと「無効」になる。

#### `on_running` の `"ask"`

`[window] on_running = "ask"` は、同じ exe が既に動いていてメインウィンドウを前に出せるときに、
起動もアクティブ化もせず確認する。`"activate"` はそのまま前に出し、`"new"` は常に新しい
インスタンスを起動する。確認では「Ir para ele」(実行中のプログラムへ切り替え)・
「Abrir outra vez」(別インスタンスを起動)・「Cancelar」を選ぶ。使用回数は選んだ後だけ記録する。
CLI (`noh ql run`) には画面が無いため、`"ask"` は `"activate"` として扱う。

JSON Schema は [`quicklaunch.schema.json`](./quicklaunch.schema.json) にコミットしており、
VS Code (Even Better TOML 拡張) などで補完と検証に使える。`config.toml` と同じく
`schemars` から生成し、キーをソートして Rust 固有の integer `format` を落とすため
byte-for-byte で再現できる:

```bash
cargo run -p nohrs-core --example quicklaunch_schema > docs/quicklaunch.schema.json
```

`nohrs-core` のユニットテスト (`the_committed_schema_is_up_to_date`) が、コミット済みファイルと
生成結果の一致を `cargo test` のたびに確認する。仕様と実装の詳細は
[`launcher.md`](./launcher.md) の「クイック起動」節を参照。

---

## 3. ロード戦略 (4 層 override)

| 優先度 | 層 | 例 |
|-------|----|----|
| 最低 | デフォルト値 (コード内 `Default::default()`) | — |
| 中 | 設定ファイル | `~/.config/nohrs/config.toml` |
| 高 | 環境変数 | `NOHRS_THEME=dark` |
| 最高 | CLI 引数 | `--theme dark` |

実装は `serde` + 手動マージで P1 は十分 (`figment` 等の依存追加は不要)。

```rust
// 擬似コード
let defaults = Config::default();
let from_file = Config::load_toml(path)?;
let from_env  = Config::from_env();
let from_cli  = Config::from_clap(args);

let merged = defaults
    .merged_with(from_file)
    .merged_with(from_env)
    .merged_with(from_cli);
```

---

## 4. JSON Schema 生成

`schemars` crate で Rust 構造体 (`Config` 以下) から JSON Schema を自動生成し、`docs/config.schema.json` にコミットする。出力は安定化済み (キーソート + Rust 固有の integer `format` を除去) なので byte-for-byte で再現できる。

```bash
# GUI バイナリ経由 (macOS):
cargo run --bin LumaFiles -- config schema > docs/config.schema.json

# GUI 非依存 (Linux/CI、上と同一バイト列を出力):
cargo run -p nohrs-core --example schema > docs/config.schema.json
```

**CI で常に最新を保証**: `config schema` ジョブが上記 example でスキーマを再生成し、コミット済みファイルと `git diff --exit-code` する。構造体を変更してスキーマ再生成を忘れると、diff 付きで CI が落ちる。加えて `nohrs-core` のユニットテスト (`committed_schema_is_up_to_date`) が同じ検証をローカル/`cargo test` でも行う。

config.toml の冒頭に:

```toml
#:schema https://nohrs.app/schema/config.schema.json
```

を埋め込み、VS Code (Even Better TOML 拡張) 等でオートコンプリート対応。

---

## 5. Hot Reload

| 項目 | hot reload | 実装フェーズ | 備考 |
|------|-----------|-------------|------|
| `theme.mode` | ✅ Yes | **P1** | `notify` でファイル監視、即時反映 |
| `theme.accent` | ✅ Yes | **P1** | 同上 |
| `ui.default_sort` | ✅ Yes | **P1** | 開いているビューに伝播 |
| `ui.default_sort_descending` | ✅ Yes | **P1** | 開いているビューに伝播 (表示を保存済みのフォルダーは除く) |
| `ui.show_hidden` | ✅ Yes | **P1** | 開いているビューに伝播 |
| `ui.icon_pack` | ✅ Yes | **P1** | アイコンキャッシュをクリア |
| `diagnostics.runtime_log` | ❌ No (再起動) | 默认 `false`；设置 → 关于 → 运行日志 | GUI 启动时决定是否创建文件日志和 panic 日志 |
| `diagnostics.store.*` | ❌ No (再起動) | P3 で hot reload に格上げ検討 | ストア接続 open 時に profile フックの登録可否を決めるため (off 時はフック未登録でオーバーヘッド 0) |
| `keybindings.*` | ❌ No (再起動) | P3 で hot reload に格上げ検討 | キーマップは入力ハンドラに焼き込まれているため |
| `plugins.enabled` | ❌ No (再起動) | P5 で動的 enable/disable に格上げ検討 | wasm host のライフサイクル安定化が前提 |
| `schema_version` | ❌ No (再起動、マイグレーション処理走る) | 永続 | 変更は版アップに伴う |

**実装メモ**: P1 で `notify` を最小利用 (`nohrs-core::config::watcher`)。検索系の watcher は P3 で復活するが、config 監視は別パスで先行。

---

## 6. バリデーション

| 種類 | 動作 |
|------|------|
| **構文エラー (parse 失敗)** | 起動時に **panic せず** デフォルト config で起動。エラーメッセージを UI のステータスバーに表示し、ファイルパスを案内 |
| **未知の key** | warn ログ + 無視 (forward-compat) |
| **未知の `schema_version`** | 起動時に「nohrs の新しいバージョンが必要かもしれません」を表示、デフォルト config にフォールバック |
| **値の範囲外** | warn ログ + デフォルト値を使う |

---

## 7. マイグレーション

`schema_version` を bump するときは:

1. 旧 schema の serde 構造体を別 module (`legacy_v1`) に残す
2. ロード時に `schema_version` を peek して該当 module で deserialize
3. `Migration::v1_to_v2()` で新 schema に変換、`config.toml` を上書き保存
4. 上書き前にバックアップ (`config.toml.bak-<timestamp>`) を作成

---

## 8. CLI サブコマンド (P1 から)

```bash
nohrs config show          # 現在の merged config を JSON で表示
nohrs config edit          # $EDITOR で config.toml を開く
nohrs config validate      # schema 検証
nohrs config schema        # JSON Schema を stdout に
nohrs config path          # config.toml のフルパスを表示
nohrs config reset         # config を初期化 (backup を取る)
```
