# Launcher

> Status: Draft (P3 で実装)
> Related: [`ROADMAP.md`](./ROADMAP.md), [`docs/search.md`](./search.md), [`docs/plugin-api.md`](./plugin-api.md)

本書は Raycast 風グローバルランチャーの設計を定めます。Launcher × Explorer の "Launcher" 側です。

---

## 1. ウィンドウモデル

| 観点 | 仕様 |
|------|------|
| **モデル** | 別ウィンドウ (フローティング)。explorer とは独立した window |
| **表示位置 (初回)** | 画面中央寄り上 (画面上端から 25%) |
| **表示位置 (2 回目以降)** | 最後にユーザーが置いた位置を記憶し、その位置で起動 |
| **位置の記憶** | redb ホスト KV (`state.redb`、key `launcher.window_position`) に保存。高頻度更新のため config.toml ではない (詳細は [`persistence.md`](./persistence.md)) |
| **移動** | 検索バー上部の数十 px (drag handle) をマウスでドラッグして移動可能 |
| **位置リセット** | `Cmd+0` でデフォルト位置に戻す |
| **マルチディスプレイ** | ドラッグで別ディスプレイへ移動可、移動先で位置記憶 |
| **サイズ** | **750 × 500** 固定 (リサイズ不可、検索 UI なので幅固定で十分) |
| **デコレーション** | borderless + 丸角 + blur background (mica / vibrancy) |
| **フォーカス喪失時** | 自動 close (`ESC` でも close)、設定で disable 可 |
| **再 hotkey** | 既に開いている場合は toggle (close)。位置は記憶のまま |
| **起動アニメ** | fade-in 100ms |

---

## 2. 起動キー

| キー | デフォルト | 動作 |
|------|-----------|------|
| **グローバルホットキー** | `Cmd+Shift+Space` | OS グローバル登録、別アプリ前面でも launcher を起動 |
| **アプリ内ショートカット** | `Cmd+K` | explorer focus 時に launcher を起動 |
| **menubar アイコン** | (P5 で検討) | 常駐 process が必要 |

`Cmd+Space` は macOS Spotlight 占有のため奪わない (デフォルト)。設定で上書き可。

### 実装

- crate: **`global-hotkey`** (Carbon 経由、pure Rust、tray-icon 系と同 author)
- macOS: Carbon Hot Keys API、Linux: X11 / Wayland

---

## 3. ホーム画面 (起動直後・入力前)

**Super minimal + placeholder ヒント** を採用:

- 検索バーのみ表示
- placeholder: `Search files or commands…    ? for commands, > to run`
- 結果リストは空 (入力するまで何も表示しない)

理由:
- ノイズゼロ (Spotlight 流の美しさ)
- 学習導線として placeholder にヒントを書く (`?` / `>` の入り口)

---

## 4. アクションフレームワーク

### 4.1 `Command` trait

```rust
// crates/nohrs-launcher/src/command.rs

pub trait Command: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn title(&self) -> SharedString;
    fn subtitle(&self) -> Option<SharedString>;
    fn icon(&self) -> Icon;
    fn keywords(&self) -> &[&'static str];
    fn category(&self) -> Category;
    fn mode(&self) -> Mode;
    fn arguments(&self) -> &[ArgSpec];
    fn default_hotkey(&self) -> Option<KeyChord>;
    fn execute(&self, ctx: &CommandContext, args: &Args) -> CommandResult;
}

inventory::collect!(&'static dyn Command);
```

`inventory` クレートで linker-time にコマンドを集める。各 crate (`nohrs-pages`, `nohrs-services`, `nohrs-launcher` 自体) が自身のコマンドを `inventory::submit!` で宣言。

### 4.2 メタデータ

| フィールド | 説明 |
|-----------|------|
| `id` | グローバルユニーク (`"explorer.open_path"` 等) |
| `title` | 主表示 |
| `subtitle` | 補助情報 (optional) |
| `icon` | 16-24px、SF Symbols 互換 or 自前 SVG |
| `keywords` | 検索マッチ強化用 (例: "calc", "math") |
| `category` | "Productivity", "Developer Tools", "Media", "Cloud", "Theme" |
| `mode` | `Instant` (即実行)、`View` (結果を launcher 内に表示)、`External` (別 window 開く) |
| `arguments` | `Vec<ArgSpec>` (string / path / number / enum)、検索バーに inline 入力 `> command arg1 arg2` |
| `default_hotkey` | 任意のコマンド固有グローバルホットキー |

### 4.3 plugin command (P4)

`plugin.toml` の `[[commands]]` セクションで宣言、WIT 経由 `run_command(id, args, ctx)` で呼び出し。host 側で `Command` trait の adapter 実装で `inventory` レジストリに登録するので、コア plugin と同じレジストリで検索可能。

詳細は [`docs/plugin-api.md`](./plugin-api.md) §commands interface。

---

## 5. 結果リスト

### レイアウト

```text
┌────────────────────────────────────────────────────┐
│ 🔍  [icon]  Title text             [kind]   ⌘K    │
│             Subtitle text                          │
└────────────────────────────────────────────────────┘
```

| 要素 | 説明 |
|------|------|
| icon | 16-24px |
| title | 主表示 (ファイル名 / コマンド名) |
| subtitle | 補助情報 (フルパス / 説明 / 最終更新) |
| kind badge | "File" / "Folder" / "Command" / "Plugin" / "Calc" 等 (右寄せ、subtle) |
| accessory | ショートカット表示 (右端、subtle) |

### Section 分け

- "Recent" (使用履歴)
- "Files" (検索結果のファイル)
- "Commands" (検索結果のコマンド)
- "Calculations" (式の自動評価結果)
- "Plugins" (plugin が提供する候補)

各セクション折りたたみ可。

### ハイライト

マッチした文字を bold + accent カラーでハイライト。

---

## 6. ランキング (nucleo + boost)

| 観点 | 仕様 |
|------|------|
| マッチャー | **`nucleo` crate** (helix-editor 製、高性能 fuzzy matcher) |
| 基本スコア | nucleo の scoring 関数 |
| recency boost | `last_used_at` が新しいほどスコア加算 |
| frequency boost | `use_count` が多いほどスコア加算 |
| context boost | 現在の context (`explorer focused` / `launcher only`) に合致するカテゴリを優先 |
| 永続化 | SQLite `history` テーブル + `command_usage` テーブル (`kind=command` で記録) |

---

## 7. 詳細ペイン

**必要時のみ表示** (デフォルトは結果リストのみで動作軽快)。

| トリガ | 動作 |
|--------|------|
| `Tab` | 詳細ペインを open/close toggle |
| file 選択時 | ファイル preview ペイン (画像/テキスト/メタ) |
| command 選択時 | コマンド説明 + 引数フォーム |

詳細ペインの中身は plugin から push 可能 (`launcher.push-view`)。詳細は [`docs/plugin-api.md`](./plugin-api.md) §`view-node`。

---

## 8. ナビゲーション (push-pop モデル)

Raycast 流のスタック型ナビ:

| 操作 | 動作 |
|------|------|
| コマンド実行 (mode=View) | スタックを push、新しいビューを表示 |
| `ESC` | 1 ステップ pop、最後の pop で launcher close |
| `Cmd+W` | スタッククリアして launcher close |
| 矢印キー | 結果リスト内の移動 (push しない) |
| breadcrumb top-bar | スタックの history を表示 |

---

## 9. 初期コアコマンド (P3 で実装、15-20 個)

| 領域 | コマンド |
|------|---------|
| File operations | `Open Path`, `Reveal in Finder`, `Quick Open File`, `Open Recent` |
| Navigation | `Go to Path`, `Bookmark Path`, `Recent Folders` |
| Quick calc | `Calculator` (入力が数式なら自動)、`Unit Convert` |
| System | `Quit`, `Settings`, `Reload`, `About` |
| Search | `Search Files` (current scope), `Search Content` (Tantivy 全文検索) |

---

## 10. アクション (item 単位)

各結果アイテムに `Vec<Action>` を持ち、右クリックメニュー or `Enter` (primary) / `Cmd+Enter` (secondary) で起動:

```rust
struct Action {
    id: String,
    title: String,
    shortcut: Option<KeyChord>,
    icon: Option<Icon>,
}
```

例: ファイル選択時のアクション:
- Open (Enter, default editor)
- Reveal in Explorer (Cmd+Enter)
- Copy Path (Cmd+Shift+C)
- Open in Finder (Cmd+R)

---

## 11. 実装上のポイント

- crate: **`nohrs-launcher`** (P3 で新規)
- window: GPUI で borderless + blur 化、`window.set_window_level(.floating)`
- global hotkey: `global-hotkey` crate
- ranking: `nucleo`
- 位置記憶: `nohrs-store::KvStore` (redb `state.redb`) 経由

---

## 12. パフォーマンス目標

- グローバルホットキーから launcher window 表示: **<100ms**
- キー入力から結果リスト更新: **<50ms** (debounced)
- 検索結果取得 (全文検索含む): **<500ms** 中央値

---

## 13. 実装状況

`crates/nohrs-launcher` に「検索」部分を実装済み。本書の残りは未実装。

### 実装済み

| 項目 | 実体 |
|------|------|
| グローバルホットキー (§2) | `hotkey.rs`。既定 `Cmd+Shift+Space` (macOS) / `Ctrl+Shift+Space` (他)。セッションに応じて 2 経路を使い分ける (下記) |
| アプリ内ショートカット (§2) | `Cmd+K` / `Ctrl+K`。binary 側で `ToggleLauncher` action に bind |
| ホーム画面 (§3) | 入力前は結果ゼロ、placeholder のみ |
| 検索欄 | `field.rs`。gpui 直書きの 1 行入力。IME (preedit の下線表示・確定まで検索しない・変換中は ↑↓/Enter を IME に譲る)、grapheme 単位のカーソル、選択・クリップボード対応 |
| ウィンドウ (§1) | borderless + 角丸 + 透過。`gpui_component::Root` を使わないことで実現 (下記) |
| 結果リスト (§5) | icon / title / subtitle / kind badge、マッチ文字のハイライト |
| ランキング (§6) | `nucleo-matcher` + exact / prefix / 深さ boost |
| ファイル名インデックス | `nohrs-services::search::file_index`。起動時に home を走査して常駐。`Scanning` / `Ready` / `Failed` を UI に出し分ける |
| アクション (§10) | `Enter` = Open、`Cmd/Ctrl+Enter` = Reveal |

#### グローバルホットキーの 2 経路

Wayland には X11 の passive grab に相当するものが**意図的に**ない (それはキーロガーが欲しがる能力そのもの)。
そのため OS ではなくセッションで分岐する:

| セッション | 経路 | 実体 |
|-----------|------|------|
| macOS / X11 / Windows | passive grab | `global-hotkey` crate (Carbon / `XGrabKey` / hook) |
| Wayland | desktop portal | `org.freedesktop.portal.GlobalShortcuts` を `ashpd` 経由。コンポジタが binding を所有し、ユーザーが承認する |

Wayland では grab へフォールバック**しない**。XWayland クライアントからの grab は
「登録は成功するが X11 ウィンドウ上でしか発火しない」という、動いているように見えて動かない
最悪の状態になるため。portal が無い環境では警告を出してアプリ内 `Cmd+K` に委ねる。

portal 側は Wayland コンポジタ無しでは実機確認できないため、
`tests/wayland_portal.rs` がプライベート D-Bus 上にモックポータルを立て、
`CreateSession` → `BindShortcuts` → `Activated` が summon まで届くことを検証している。

### 未実装

`Command` trait とコマンド (§4、§9)、詳細ペイン (§7)、push-pop ナビ (§8)、
セクション分け (§5 の Recent / Commands / Calculations)、使用履歴 boost の永続化 (§6)、
ウィンドウ位置の記憶 (§1)、フォーカス喪失時の自動 close (§1)、ホットキーの設定による上書き (§2)。

### 既知の制約

- **blur (mica / vibrancy) は未対応** (§1)。ウィンドウは透過 + 角丸だが、背面のぼかしはまだ。
  コンポジタが無い環境では透過部分が背面の内容そのままになる (パネル自体は不透明なので、
  最悪でも角が丸く見えないだけ)。
- **Wayland の portal は実機未検証。** プロトコル上のやり取りはモックポータルで検証済みだが、
  GNOME / KDE 実機での承認ダイアログ周りは未確認。
- **IME は実機未検証。** gpui の `EntityInputHandler` に沿って実装し、
  preedit の保持・確定・UTF-16 オフセット変換は単体テストで確認しているが、
  headless 環境では実際の IME を駆動できないため、日本語入力の通し確認は未実施。
- **Linux 常駐には 1px の keep-alive ウィンドウが必要。** gpui の Linux バックエンドは
  最後のウィンドウが閉じた時点でイベントループを止める (`x11/client.rs`) ため、
  `nohrs launcher` は不可視ウィンドウを 1 つ保持してプロセスを生かしている。macOS では不要。
- **インデックスは起動時の 1 回きりで、ファイルシステムを追従しない。**
  起動後に作られたファイルは次回起動まで検索に出ず、消えたファイルは残る。
  content index 側の watcher に繋ぐのは今後の作業。
- **ホーム画面はフッターを出したまま。** §3 は「検索バーのみ」だが、ウィンドウは 750×500 固定なので
  入力前も同じ高さのパネルが出る (結果リストは空)。フッターだけ隠しても §3 にはならず、
  `Enter` / `Cmd+Enter` の唯一の導線が消えるだけなので、今は常時表示に倒している。
  §3 に本当に寄せるには Spotlight / Raycast のように入力に応じてウィンドウを伸縮させる必要があり、
  それは今後の作業。
- **Windows のパス区切りは未対応。** `src/main` のようなパスクエリは `/` を前提にしており、
  `\` 区切りのパスとは一致しない。ADR 0002 のとおり当面 Windows は対象外。
- **Wayland の activation token を渡していない。** `Activated` シグナルが持つトークンを
  ウィンドウ生成に引き渡せば確実にフォーカスを得られるが、gpui 0.2 に
  そのための公開 API がない (トークンは gpui 内部で管理されている)。

---

## 14. クイック起動 (Quick Launch)

> 仕様: [`QuickDock-技术实现方案与边界约定.md`](./QuickDock-技术实现方案与边界约定.md) §17.1 E。M4 は同 §17.2。
> M1–M2 実装済み。M3 の設定ページ・CLI はそれぞれ `settings/quick_launch.rs`・
> `nohrs-cli/src/quick_launch.rs` に追加され、`@` 検索は §14.5 の設計で実装される。
> M4 の `uwp` / `command` / `group` / `on_running = "ask"` / 動的分組は §14.6–§14.10、
> 仕様との差異は §14.11 にまとめる。

Launcher とは別の、登録したショートカットを格子または一覧で並べる独立ウィンドウ。
グローバルホットキーで呼び出し、常駐して何度でも開閉する。設定は `config.toml` ではなく
独立した `quicklaunch.toml` に置く (§14.2)。

### 14.1 モジュール

| ファイル | 役割 |
|---|---|
| `crates/nohrs-core/src/config/quick_launch.rs` | データモデル (`QuickLaunchConfig` / `WindowSettings` / `Group` / `Item`)、パース、検証、マイグレーション、`to_toml`。ファイル I/O はしない |
| `crates/nohrs-core/src/config/paths.rs` | `quick_launch_file()` と `quick_launch_icon_dir()` |
| `crates/nohrs-services/src/launch.rs` | `LaunchAction` trait、`LaunchRegistry`、パス変数展開、URL 検証、エラー型。非 Windows はすべて `Unsupported` |
| `crates/nohrs-services/src/launch/windows.rs` | 組み込み種別の `ShellExecuteExW` 実装、`IApplicationActivationManager` による `uwp`、`std::process::Command` による `command`、`on_running` のウィンドウ活性化 |
| `crates/nohrs-pages/src/quick_launch.rs` | `QuickLaunch` グローバル状態 (唯一の状態源)、ファイルの読み書きと監視、ウィンドウの生成・表示・非表示、アイコンのキャッシュ、起動用スレッド、常駐の判定、`command` の初回確認と信頼記録、`group` の展開と一括起動、動的分組の生成 |
| `crates/nohrs-pages/src/quick_launch/view.rs` | タイトルバー、グループバー、グリッド / リスト、キーボードとマウス、右クリックメニュー、ドラッグ & ドロップ、編集カード (§14.11)、`command` / `group` / 実行中プログラムの確認浮層 (§14.7–§14.9) |
| `crates/nohrs-launcher/src/hotkey.rs` | 複数ホットキーの登録・解除・パース (§10) |
| `crates/nohrs/src/app.rs` | 主モードと常駐モードでの初期化、`ToggleQuickLaunch` のハンドラ、ホットキーのフック |
| `crates/nohrs-pages/src/tray.rs`、`root.rs` | トレイメニューの項目と、常駐時のウィンドウを閉じる振り分け (§14.4) |
| `crates/nohrs-pages/src/settings/quick_launch.rs` (M3) | 設定ページ。M4 で種別 `uwp` / `command` / `group`、動的分組の作成、信頼の表示と取り消しを追加 |
| `crates/nohrs-cli/src/quick_launch.rs` (M3) | `noh quick-launch` (別名 `noh ql`)。M4 で `apps`、`--uwp`、`--yes` と動的分組の扱いを追加 |

フォルダー (`folder`) の起動は services が自分で開かず `OpenFolder(path)` を返し、
pages 層が LumaFiles の新しいタブとして開く。`services` が UI に依存しないための分担。

### 14.2 設定ファイル `quicklaunch.toml`

`paths::quick_launch_file()`、つまり**有効な設定ディレクトリの直下**に置く。XDG なら
`$XDG_CONFIG_HOME/nohrs/quicklaunch.toml` (未設定時は `~/.config/nohrs/quicklaunch.toml`)、
ポータブルモードなら `userdata/quicklaunch.toml`。アイコンのディスクキャッシュは
`paths::quick_launch_icon_dir()` (`cache_dir()/quick_launch_icons`) に置く。

```toml
schema_version = 1

[window]
hotkey = "ctrl+alt+Space"   # global-hotkey の形式。空文字なら登録しない
view = "grid"               # "grid" | "list"
columns = 8                 # 2..=16
cell_size = 100             # 64..=160
icon_size = 64              # 16/24/32/48/64 のどれか (他は最も近い値)
list_columns = 2            # 1..=3
auto_hide = true
topmost = false
close_after_launch = true
on_running = "activate"     # "activate" | "new" | "ask" (§14.9)
keep_running = true         # メインウィンドウを閉じてもプロセスを残す (§14.4)

[[group]]
id = "general"
name = "Geral"

[[group.item]]
slot = 0
type = "app"                # app | folder | file | url | uwp | command | group | plugin:<id>
name = "Editor"
path = "%ProgramFiles%\\Notepad++\\notepad++.exe"
```

- ファイルが無いときは `general` という空のグループ 1 つを持つ既定設定を**メモリ上だけ**で使い、
  最初の編集まで書き込まない。
- パースできない、または `schema_version` がこのビルドより新しいときは**読み取り専用**にし、
  ウィンドウ上部に理由を出す。書き戻すと新しいビルドのフィールドを消してしまうため。
- 値の範囲外は丸めて警告、`id` と `name` の不正・スロットの重複などは該当グループ / 項目を
  捨てて警告にする (規則は仕様 §4.2)。警告はログにだけ出し、画面は止めない。
- ウィンドウや設定ページでの編集は**ファイル全体を書き直す**ので、手書きのコメントは失われる。
  保存は 300 ms にまとめてから、一時ファイル → `fsync` → `.bak` → rename の順で行う。
- `notify` で監視し、内容のハッシュが変わったときだけ読み直す。UI 自身の保存による通知は
  ハッシュ比較で無視する。外部で編集した場合も、次にウィンドウを開いた時点のファイルが正しい。
- `[[group]]` に `source` を書くと、そのグループの項目はファイルではなく実行時に生成される
  動的分組になる (`folder:` / `recent` / `most_used`、§14.10)。`command` の `extra`、
  `on_running = "ask"` を含む各フィールドの意味は [`config.md`](./config.md) の
  「クイック起動」節にまとめてある。

### 14.3 呼び出し

| 入口 | 実体 |
|---|---|
| グローバルホットキー (既定 `Ctrl+Alt+Space`) | `quicklaunch.toml` の `window.hotkey` を起動時に登録。押すたびに表示 / 非表示を切り替える |
| トレイメニュー | 「Inicialização rápida」で `ToggleQuickLaunch` を dispatch |
| `ToggleQuickLaunch` action | 設定ページや CLI などアプリ内の入口から dispatch |

- 既定値は `ctrl+alt+Space`。Launcher の `Ctrl+Shift+Space` と**同じ組み合わせは登録しない**
  (登録に失敗したら以前のホットキーに戻し、設定ページに理由を出す)。
- `window.hotkey` が空文字なら OS には登録しない。トレイと action からは開ける。
- 登録は主モードと常駐モードの両方で、同じ初期化関数から行う。常駐モードで既にメインの
  LumaFiles が同じホットキーを握っている場合は、後から起動した側は `info` ログだけ残す。
- ウィンドウは**初回の呼び出しで 1 つ作る**。以後は表示 / 非表示を切り替えるだけで、
  閉じても破棄しない。gpui の Windows 実装にはウィンドウ単位の hide が無いため、
  `tray::platform` の `ShowWindow` を直接使う (`tray::show` / `tray::hide` はメインウィンドウの
  トレイ状態まで変えるので使わない)。
- 既定サイズはグリッド 980×875、リスト 660×860 (論理ピクセル)、最小 360×300。位置は
  ディスプレイごとに `state.redb` へ記憶し、復元先がどのディスプレイにも無ければマウスのある
  ディスプレイに中央表示する。
- `Esc` は絞り込みがあればまず消し、無ければ隠す。フォーカスを失って 150 ms で自動的に隠すが、
  `auto_hide = false` とピン留め (`topmost = true`) のときは隠さない。
- アイコンは `quick_launch_icon_dir()` に `png` でキャッシュし、メモリ上は LRU で 512 個まで。
  抽出は専用スレッドで行い、起動スレッドとは分ける (ネットワークパスのアイコンが固まっても
  起動は待たない)。

### 14.4 常駐 (プロセスを残す)

`keep_running = true` (既定) かつホットキーの登録に成功したときだけ常駐する。
メインウィンドウを閉じたときの挙動は `close_action` で決める:

| `close_to_tray` | 常駐 | 結果 |
|---|---|---|
| true | 任意 | 従来どおりトレイに隠す |
| false | true | **隠す** (破棄しない)。RootView・IPC・`state.redb` を残し、次に `LumaFiles.exe` を起動すると既存プロセスがウィンドウを出す |
| false | false | 未保存の変更を書いてから終了する |

- 常駐中はタイトルバーの「⋯」メニューから「Mostrar janela」でメインウィンドウを出し、
  「Sair do LumaFiles」でプロセスごと終了できる (`tray_icon = never` でも出口がある)。
- トレイアイコンの設定 (`always` / `background` / `never`) は従来のロジックのまま。
- `LumaFiles launcher` の常駐モードでも同じウィンドウを出す。このモードには Explorer・トレイ・
  IPC サーバーが無く、`state.redb` も開かない (単一プロセスの排他ロックのため)。そのため
  ウィンドウ位置と使用回数は記録せず、ウィンドウは毎回中央に出す。
- 常駐モードでフォルダーを開くときは、まず `ipc::request_open` で動いているメイン LumaFiles に
  渡し、居なければ `nohrs` 自身をパス付きで起動する。ショートカットの起動はそのまま動く。
- 主モードと常駐モードが同時に動くと `quicklaunch.toml` を両方が編集しうる。マージはせず、
  後から保存した側が勝つ (原子書き込み + ハッシュ監視で壊れはしない)。

### 14.5 `@` 検索 (M3)

Launcher の入力が `@` で始まるときだけ、ファイル検索の代わりにクイック起動の項目を返す。
`nohrs-launcher` は `nohrs-pages` に依存できない (§3.4) ため、M1 の `HotkeyHooks` と同じ
**フック**で繋ぐ:

```rust
// crates/nohrs-launcher
pub struct QuickLaunchEntry {
    pub group: SharedString,
    pub slot: u32,
    pub name: SharedString,
    pub tags: Vec<SharedString>,
    pub kind: SharedString,
    pub path: SharedString,
}

#[derive(Clone, Copy)]
pub struct QuickLaunchHooks {
    pub snapshot: fn(&App) -> Vec<QuickLaunchEntry>,
    pub activate: fn(&mut App, group: &str, slot: u32, reveal: bool),
}

pub fn set_quick_launch_hooks(cx: &mut App, hooks: QuickLaunchHooks);
```

- `nohrs/src/app.rs` が主モードと常駐モードの両方でフックを登録し、pages の
  `launcher_snapshot` / `activate_by_id` に転送する。`snapshot` は前面スレッドでメモリ上の
  データを clone するだけ。
- `ranking::ItemKind` に `QuickLaunch` を追加し、badge は `t!("Inicialização rápida")`、
  アイコンは既存の起動系アイコンを使う。`LauncherItem` に
  `quick_launch: Option<(SharedString, u32)>` を足し、ファイル検索の結果は常に `None`。
- `@` を除いた文字列で、名前とタグを `nucleo` でマッチする。並び順はスコア → グループ順 →
  スロット順、最大 `MAX_RESULTS` 件。**このときファイルは検索しない**。
  `@` だけのときは全項目をグループ順・スロット順で列挙する。
- `@` で始まらないクエリは今までと完全に同じ (重み付けも含めて変更しない)。
- 選択項目の `Enter` / クリックは `activate(group, slot, false)`、`Shift+Enter` / `Shift+クリック` は
  `activate(group, slot, true)` (LumaFiles で所在を開く)。どちらも Launcher を閉じる。
  グループ id が存在しない場合は何もしない。
- placeholder の既存ヒントの後ろに `t!("@ para a inicialização rápida")` を足す。

### 14.6 `uwp` (UWP アプリ、M4)

`uwp` は Microsoft Store 形式のパッケージアプリを AppUserModelID (AUMID) で起動する種別。
`path` に `Microsoft.WindowsCalculator_8wekyb3d8bbwe!App` のような AUMID を書き、`args` を
起動引数として渡す。`cwd` と `admin` は意味を持たず、ファイルに書かれていても無視する。

- 起動は `IApplicationActivationManager::ActivateApplication`。COM は起動スレッド
  (`quick-launch`) が既に持っている apartment を使う (§8.4)。
- 健全性チェックは書式だけを見る: `!` の両側が非空、空白文字を含まない、256 文字以内。
  インストール済みかどうかは問い合わせない (§8.1 の 5 ms 予算)。書式が不正な項目は
  タイルがグレーになり、起動もしない。
- 設定ページの種別に「Aplicativo UWP」が増える。`uwp` を選ぶと「Caminho」行が
  「ID do aplicativo」(アプリ ID) に変わり、「Procurar…」が「Escolher aplicativo…」になる。
  押すと Apps フォルダーを**バックグラウンドで 1 回だけ**列挙した一覧が開き、名前で絞り込める。
  選ぶと AUMID を、名前が空ならアプリ名も入力欄に入れる。AUMID を持つクラシックな
  デスクトップアプリも一覧に出る (どちらも `ActivateApplication` で起動できる)。
- コマンドラインでは `noh ql apps [--json]` が名前と AUMID を一覧し、
  `noh ql add --uwp <AUMID>` が `<パス>` の代わりに `uwp` 項目を追加する (`--name` を
  省略すると名前は AUMID になる)。

### 14.7 `command` (シェルコマンド、M4)

`command` は `extra` の `shell` と `script` でシェルスクリプトを起動する。`path` と `args` は
持たない (`args` は無視される)。

```toml
[[group.item]]
slot = 1
type = "command"
name = "Build"
cwd = "%USERPROFILE%\\src"

[group.item.extra]
shell = "pwsh"      # "cmd" | "pwsh" | "powershell" | "python"
script = "npm run build"
hidden = false      # true: ウィンドウを出さない (CREATE_NO_WINDOW)
keep_open = false   # true: 終了後もコンソールを開いたままにする
```

- 起動は `std::process::Command`。`cmd` は `%ComSpec%` (無ければ `cmd.exe`) に
  `/d /s /c`、`pwsh` / `powershell` は `-NoLogo -NoProfile -EncodedCommand`
  (UTF-16LE の base64)、`python` は `.arg(script)`。`hidden` は `CREATE_NO_WINDOW`、
  指定が無ければ `CREATE_NEW_CONSOLE` で自分のコンソールを持つ。`keep_open` は `cmd` の
  `/k`、PowerShell の `-NoExit`、Python の `-i` になる。
- 作業ディレクトリは `cwd` を展開したもの。未設定なら `%USERPROFILE%`。
  起動したプロセスは待たず、すぐ `Started` を返す。
- `admin = true` のコマンドはこの期では未対応 (D4)。健全性チェックは「無効」、
  起動は「未対応」を返し、設定ページは「Executar como administrador」をヒント付きで
  無効表示にする。
- 健全性チェックは `extra` の内容と、設定された `cwd` が存在するか。
  シェルが `PATH` にあるかは見ない (遅いため)。

#### 初回確認と信頼

- まだ信頼していない `command` を起動しようとすると、ウィンドウ内の浮層で確認する。
  内容は項目名・シェル・作業ディレクトリ・スクリプト全文 (等幅、スクロール可、
  高さはウィンドウの 60% まで)・「Executar」/「Cancelar」。`hidden` のときは
  「バックグラウンドで実行され、ウィンドウが見えない」旨を警告色で出す。
- 「Executar」で信頼を記録してから起動する。「Cancelar」は何も起動せず、エラーも出さない。
- 信頼の記録は `shell` / `script` / ファイルに書かれた `cwd` / `hidden` / `keep_open` の
   SHA-256 の先頭 32 hex を KV の `trusted_<32hex>` に置く。スクリプトやフラグを 1 文字でも
  変えると再確認になる。記録は項目を移動・削除しても残り、`prune_usage` は `usage_*` だけを
  掃除する。
- ウィンドウが隠れているとき (Launcher の `@` から起動した場合など) は、確認の前に
  ウィンドウを表示する。
- 常駐モードで `state.redb` を開けない場合は信頼をメモリ内にだけ持ち、プロセス終了で失効する。
  その場合ダイアログの下に「Sem armazenamento: a confirmação vale até fechar o programa.」を出す。
- 設定ページのフォームから保存した `command` だけは、その保存で自動的に信頼される (D5)。
  ファイルの手編集・CLI・ホットリロードで入った変更は自動では信頼されない。
- 設定ページは「Confiável」/「Não confiável」と「Revogar confiança」(信頼を取り消す) を出す。
- グループの中の未信頼の `command` は 1 つの確認にまとめる (§14.8)。

### 14.8 `group` (複数項目のまとめ起動、M4)

`group` は同じグループ内の項目を `id` で参照し、順に起動する種別。`path` / `args` /
`cwd` / `admin` は持たない。

```toml
[[group.item]]
slot = 2
type = "group"
name = "Work"
items = ["editor", "script"]

[[group.item]]
slot = 3
type = "app"
name = "Editor"
id = "editor"
path = "%ProgramFiles%\\Notepad++\\notepad++.exe"
```

- 参照される項目には `id` が要る。設定ページでグループを保存すると、選んだ項目に
  `item_<スロット>` が自動で割り当てられる (衝突時は `_2`、`_3`…)。手書きの `id` は
  `^[a-z0-9_]{1,32}$`。
- 展開規則: ネストした `group` は深さ 8 まで再帰的に展開し、訪問済みはスキップ、
  起動するのは最大 16 項目。超過分は起動せず、下部に
  「{skipped} itens do grupo excedem o limite de {limit}…」を出す。別グループの `id` は
  無視する。有効な参照が 1 つも無いときは「Este grupo não tem itens válidos.」。
- 未信頼の `command` が含まれるときは、その全部を 1 つの浮層にまとめて確認する。
  キャンセルするとグループ全体を起動しない。
- 各項目は 1 つのバッチ id で順に起動する。全応答が揃ってから、失敗があれば
  「{failed} de {total} itens não iniciaram: {names}」を 1 行だけ出し、`OpenFolder` は
  LumaFiles で順に開き、使用回数は `group` 項目にだけ 1 回記録し、`close_after_launch` は
  そのときに 1 回だけ判断する。
- グループの中では `on_running = "ask"` は `activate` として扱う (D9)。項目ごとには聞かない。
- 健全性チェックは「有効な参照が 1 つも無いとき」だけ無効。子項目の状態はそれぞれ表示する。
- 設定ページの種別に「Grupo」が増え、パス・引数・作業ディレクトリの代わりに
  「Incluir itens」(同じグループの他の項目をスロット順に並べ、クリックした順が起動順) を出す。
  クイック起動ウィンドウの編集カードでは名前だけを変更でき、他は設定ページで編集する。
- `noh ql run` はグループも実行できる。子に `command` があるときは `--yes` が必須で、
  無いとグループ全体を拒否して終了コード 1。

### 14.9 `on_running = "ask"` (M4)

`[window] on_running = "ask"` では、起動対象と同じ exe が既に動いていて、そのメイン
ウィンドウを前に出せるとき、起動もアクティブ化もせずにユーザーへ確認する。

- 浮層は「Este programa já está aberto.」と項目名を出し、ボタンは「Ir para ele」
  (実行中のプログラムへ切り替え)・「Abrir outra vez」(別インスタンスを起動)・「Cancelar」。
- 「Ir para ele」は見つかったプロセス ID を指定して起動をやり直す。メインウィンドウが
  見つからないときは新規起動に切り替える (クリックが無反応にならないように)。
- 使用回数はユーザーが選んだ後だけ記録する。「Cancelar」では記録しない。
- ウィンドウが隠れているときは、ダイアログの前にウィンドウを表示する。
- 設定ページの「プログラムが既に実行中のとき」に「Perguntar sempre」が増える。
- 対象は通常の `.exe` 起動だけ (§8.3 と同じ制約)。管理者起動と `.lnk` は常に新規起動で、
  `ask` の確認は出ない。CLI には画面が無いため、`noh ql run` では `ask` を `activate` と
  して扱う。

### 14.10 動的分組 (`folder:` / `recent` / `most_used`、M4)

`[[group]]` に `source` を書くと、そのグループの項目はファイルではなく実行時に生成される
**読み取り専用**の動的分組になる。

```toml
[[group]]
id = "tools"
name = "Ferramentas"
source = "folder:%USERPROFILE%\\bin"
```

- `source` は `folder:<パス>` / `recent` / `most_used`。それ以外の値は警告のうえ「不明」と
  して扱い、ファイルに書かれた項目を持つ普通のグループのまま残す。
- `source` のあるグループに `[[group.item]]` を書いても破棄され、警告
  (`ItemsInDynamicGroup`) が出る。保存時も項目は書き戻さず、`id` / `name` / `source` だけを書く。
- `folder:<パス>`: パス変数を展開し、**直下だけ**を読む (再帰しない)。名前の大文字小文字を
  無視して並べ、隠しファイルとシステムファイルを除き、スロット 0 から最大 512 項目。
  種類は `item_for_path` が決める。読み込みはバックグラウンドで行い、ウィンドウを表示した
  ときと、そのグループに切り替えたときに、前回から 30 秒以上経っていれば読み直す。
  ファイル監視はしない。ディレクトリが無いときは「Pasta não encontrada」を出して項目は空。
- `recent` / `most_used`: 使用履歴から生成する。`recent` は最終使用の新しい順、
  `most_used` は回数の多い順 (同数は最終使用順)。現在の設定に残っている元項目だけを対象にし、
  動的分組自身は除く。最大 `columns × 2`、32 項目まで (D7)。スロットは 0 から振り直し、
  起動と健全性チェックはコピー元の項目に記録する。ウィンドウを表示したときと、起動が
  成功するたびに作り直す。常駐モード (`state.redb` なし) では
  「Indisponível no modo residente」を出して項目は空。
- 読み取り専用: 項目の追加・移動・編集・削除・並べ替えは無効で、ドラッグの受け入れと
  「Editar」はグレー表示になる。ただし「Abrir localização」と「Copiar caminho」は使える。
  グループ自体の改名・削除・並べ替えはできる。
- 設定ページの「Novo grupo」は「Normal / Pasta / Recentes / Mais usados」を選べる。
  フォルダーはディレクトリ選択ダイアログで選ぶ。グループ一覧には `source` をそのまま表示し、
  動的分組の項目一覧は読み取り専用で
  「Itens gerados automaticamente; não podem ser editados.」を添える。
- Launcher の `@` 検索は `folder:` の生成項目を含め、`recent` / `most_used` は含めない
  (元項目と重複するため)。`prune_usage` は動的分組を有効な項目として数えない。
- CLI: `noh ql list` は動的分組を `(dynamic: <source>)` と表示して項目を出さない。
  `add` / `remove` は拒否して終了コード 1。`run` は「run its items from the window」と拒否する。

### 14.11 仕様との差異

仕様 (`QuickDock-技术实现方案与边界约定.md`) と実装が違う点、および仕様に後から追記された点:

| 項目 | 仕様 | 実装 |
|---|---|---|
| 起動失敗の表示 | 当初は通知レイヤーへの表示 | **ウィンドウ最下部の 1 行**に翻訳済みの文言 (M1 の検証で、通知レイヤーがウィンドウ更新中の入れ子更新を起こして abort したため。仕様 v2.3 §8.5 にも反映済み)。詳細はログのみ、モーダルは出さない |
| 編集 UI | 「小さなダイアログ」 | **ウィンドウ内の浮層カード** (`editor_overlay`)。「Procurar…」の非同期なパス選択結果を入力欄へ戻すにはウィンドウが要るため。「Cancelar」で閉じ、背面は操作できない |
| 右クリックのコピー | 「コピー」 | **「Copiar caminho」**。クリップボードに入るのは展開後の絶対パス (`%ENV%` / `{luma}` などを展開したもの) |
| ピン留め | v2.4 には無い。v2.5 §11.1 で追加 | タイトルバーの「⋯」の左にピンの SVG ボタン。`window.topmost` を切り替え、固定中は失焦・起動・所在表示で隠れない。読み取り専用時はセッション内だけ有効で、ツールチップに「(não será salvo)」を付ける |
| ウィンドウの隠し方 | gpui の API | Windows は `ShowWindow` を直接呼ぶ (gpui にウィンドウ単位の hide が無い)。`tray::show` / `hide` はトレイ状態を変えるため使わない |
| 健全性チェック | 初回表示後に 32 件ずつ | 初回は 1 セッションに 1 回。編集でパスが変わった項目だけ、その場で個別に再チェックする |
| グループ間ドラッグ | 対象グループの先頭の空きスロット | 仕様どおり。ただし**埋まっているスロット**へ落とした場合は、その後ろの最初の空きスロットに入れる (既存項目を上書きしない) |
| 編集時の種別 | `type` は変更できる | 変更できない。パスから `app` / `folder` / `file` を再判定する (仕様 §12.3 の 4 フィールドのみ) |
| 使用回数・ウィンドウ位置 | `state.redb` | 主モードは仕様どおり。常駐モードは `state.redb` を開けないため記録しない |
| `init` の引数 | 規定なし | 主モード / 常駐モードを区別する `Mode` を受け取る (M2 の未決問題 6 として承認済み) |
| `uwp` のアイコン | §9 はパスから抽出する | AUMID はファイルパスではないため抽出できず、タイルは種別のプレースホルダーになる。`icon` に `.ico` / `.png` を書けばその画像を使う |
| `on_running` の対象 | §17.2 F は「実行中のプログラム」 | §8.3 の制約をそのまま適用し、通常の `.exe` だけが対象。管理者起動と `.lnk` は常に新規起動で、`ask` も確認しない |
| クイック起動ウィンドウでの編集 | §17.2 E.4 は `group` を名前だけに限定 | `command` もカードでは保存できない (パスを持たず、`shell` / `script` がカードに無いため)。`command` は設定ページでのみ編集する |
| 確認ダイアログの作業ディレクトリ | §17.2 D.3「作業ディレクトリ」 | ファイルに書かれた原文を表示し、変数は展開しない。未設定は「—」 |
| 信頼キーの `cwd` | §17.2 D.1 は `cwd` を含む | 展開後ではなく**書かれた原文**をハッシュする。`%ENV%` の指す先だけが変わっても再確認しない (文面が変われば再確認する) |
| CLI の `--yes` | D6 は「コマンドラインで `command` を実行するとき」 | 実装が要求するのはグループの子コマンドだけ。単体の `command` を `noh ql run` で直接実行するときは `--yes` を要求しない |

