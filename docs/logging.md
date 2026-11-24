# Logging and measurement

> Status: Draft (P2 で `noh log` を実装、`noh perf` は後続)
> Related: [`cli.md`](./cli.md), [`persistence.md`](./persistence.md), [`config.md`](./config.md)

nohrs が「何をしたか」「どれだけ時間がかかったか」を記録し、読み返すための仕組みです。

---

## 1. 2 つのシンク

| シンク | 形式 | 読み手 | span の記録 |
|--------|------|--------|-------------|
| **stderr** | 人間可読 | ターミナルの開発者、CLI のエラーを見るユーザー | しない |
| **ローリングファイル** | **JSON Lines** | `noh log`、後続の `noh perf` | **する** |

ファイルシンクが必要な理由は単純で、**GUI には stderr の行き先が無い**からです。ウィンドウを閉じればその
セッションのログは消えます。ファイルに残して初めて「さっき何が起きたか」を後から聞けます。

Windows 版的日志直接保存在可执行文件同级目录，文件名为 `nohrs.log.<日期>`；不随启动时的工作目录变化。目录必须可写，无法写入时会报告错误并禁用文件日志。已有日志不自动迁移。无法解析可执行文件目录时，回退到原 state 日志目录。

ファイルは 1 日ずつに分かれ、`[diagnostics] log_max_file_mb` (既定 50 MB) を
超えるとその日の記録を止め、stderr に 1 度だけ警告を出す。起動時には
`log_max_total_mb` (既定 200 MB) を超えた分だけ古いファイルから削除する。

GUI 的运行日志默认关闭。在“设置 → 关于 → 运行日志”中开启后，重启软件生效；
对应配置为 `[diagnostics] runtime_log = true`。关闭后也需重启，已有日志不会删除。
关闭时不创建运行日志文件，也不安装写入 `panic.log` 的钩子；终端错误输出不受影响。
日志可能包含文件路径，分享前请检查敏感信息。独立的 `noh` CLI 日志策略不变。

Windows 以外の出力先は `$XDG_STATE_HOME/nohrs/logs/nohrs.log.<日付>` です。`state` に置くのは、ログが
**再起動をまたいで残ってほしいが、失っても困らず、バックアップもしたくない**ものだからです。
`data_dir` は DB と並ぶバックアップ対象、`cache_dir` は掃除ツールにいつ消されてもおかしくない場所で、
どちらも適しません。日次ローテーションで既定 7 世代を保持します。

ファイル名は `nohrs.log.<yyyy-mm-dd>` **ちょうど**の形をしたものだけを nohrs のログとして扱います
(`logging::is_log_file_name`)。前方一致で済ませると `noh log clear` が手元に取っておいた
`nohrs.log.backup` まで消してしまうためです。

### 1.1 プライバシー

レコードには**触ったファイルのパスと検索語がそのまま入ります**。ローカルのファイルマネージャの
記録としては意図的な設計です — どのファイルの操作が遅かったのか、どの検索が 2 秒かかったのかが
分からなければ `noh log --ops` は読む価値がありません。検索語だけをハッシュ化してもパスが残る以上
筋が通らないので、代わりに**置き場所そのもの**を境界にしています。

- ディレクトリは `0700`、ファイルは `0600` で作ります。既存のディレクトリが緩ければ起動のたびに
  締め直します (`create_log_dir` / `restrict_log_files`)。`0700` は traversal を拒否するので、
  同じマシンの別アカウントからは中のファイルのモードに関わらず届きません。
  ローテーションで新しく作られるファイルも同じで、appender は自前でファイルを開くため
  `OwnerOnly` が日付の変わった最初の書き込みで締め直します — 起動時だけだと、
  起動しっぱなしの GUI では初日しか `0600` が成立しません
- 既定で 7 世代 = 実質 1 週間で消えます
- `noh log clear --force` でいつでも空にできます
- ログはローカルのファイルにしか行きません。送信も同期もしません

ログを人に渡すときは中身を見てからにしてください。

---

## 2. 計測は span の終了レコードで行う

**これが「全操作に適用できる」根拠です。** `tracing_subscriber` のファイル層を
`FmtSpan::CLOSE` で設定してあるので、`#[tracing::instrument]` の付いた関数は**終了時に 1 レコード**を
書きます。そこに `time.busy`（実際に走っていた時間）が入ります。

したがって新しい操作を計測対象にするコストは、属性を 1 つ足すことだけです。カウンタもレジストリも
クレートごとの配線も要りません。

```rust
#[tracing::instrument(
    target = "nohrs::op",          // 計測対象が共有する唯一のターゲット
    name = "fs.move",              // 集計キー。<領域>.<動詞>
    level = "debug",
    skip_all,                      // 引数を丸ごと吐かない
    fields(src = %src.display(), dst = %dst.display())
)]
pub fn move_path(src: &Path, dst: &Path) -> Result<MoveKind> { … }
```

**規約が 3 つあります。**

- **`target = "nohrs::op"`** — クレートを問わず全ての計測操作が共有します。1 つのフィルタ指令で
  アプリ全体の計測が入り切りでき、`noh perf` が選択する対象も 1 つで済みます
- **`name = "<領域>.<動詞>"`** — `fs.move` / `search.query` / `index.build_home`。これが集計キーなので、
  安定していることが重要です
- **`level = "debug"`** — `info` に置くとディレクトリを開くたびターミナルが溢れます。ファイル側の
  既定フィルタは `info,nohrs::op=debug` なので、**stderr は静かなまま、ファイルには全操作が残ります**

`crates/nohrs-core/tests/log_file_records_operations.rs` がこの契約（名前・フィールド・`time.busy` が
既定設定でファイルに届くこと）を端から端まで固定しています。フォーマッタの設定は気付かずに変えられる
ので、設定そのものではなく結果を検証しています。

### 現在計測している操作

| 名前 | 場所 |
|------|------|
| `fs.copy` / `fs.move` / `fs.rename` / `fs.create_dir` / `fs.trash` / `fs.delete` | `nohrs-services::fs::ops` |
| `fs.list_dir` | `nohrs-services::fs::listing` |
| `search.query` | `nohrs-services::search::engine` |
| `index.build_home` / `index.remove` / `index.process_changes` | `nohrs-services::search::indexer` |

ストア層は別に、SQL 文そのものと遅いクエリの閾値検出を持っています
（`StoreLogConfig`、[`persistence.md`](./persistence.md) §5）。こちらはターゲットが
`nohrs_store::sql` / `nohrs_store::redb` で、`time.busy` ではなく SQL 本文が要るため別立てです。

---

## 3. `noh log`

```sh
noh log show                 # 直近 50 件
noh log show -n 200 --ops    # 操作だけ、200 件
noh log show --json          # 生の JSON Lines（jq に流す用）
noh log path                 # ログの場所
noh log clear --force        # 消す
```

`--ops` は span 終了レコードだけを残すので、実質「何がどれだけかかったか」の一覧になります。

```
14:22:47  fs.trash                      696µs path=/work/notes.txt
14:22:49  search.query                 12.2ms query=report scope=Home
```

CLI と GUI は**同じファイル**に追記します。`noh rm` で消したものが `noh log` に出るのはそのためです。
POSIX の `O_APPEND` は 1 行程度の書き込みでは原子的なので、両方が同時に動いても行は混ざりません。

同じファイルを共有していることが、`noh log` の実装を 2 つ縛っています。

- **`noh log` 自身はログを書きません。** `main` は購読者を入れる**前に**引数を解析し、`log` サブコマンドの
  ときだけファイルシンクを無効にします。そうしないと `show` は自分が今作ったファイルについて報告し、
  `clear` は自分のプロセスが開いているファイルを渡されます
- **`clear --force` は「今日のファイル」を unlink せず truncate します。** GUI が開いたままの
  inode を unlink すると、GUI はその後も名前の無い inode に書き続け、翌日のローテーションまで
  記録が丸ごと消えます。truncate なら `O_APPEND` の書き込みがそのまま 0 から再開します。
  ローテーション済みの古いファイルは誰も開いていないので unlink します

`--json` は `jq` に流すためのものなので、**ログが 1 つも無いときは何も出しません** (空の JSON Lines
ストリーム)。人間向けの出力では従来どおり「no log files in …」と言います。どちらも終了コードは 0 です。

---

## 4. 既知の限界

### 右键菜单诊断

文件列表中的文件、文件夹和空白区域右键菜单直接使用应用自身的菜单项，
不再预先查询 Windows Shell 扩展。`context_menu ready` 的 `shell_deferred=true`
表示走了这条路径。打开、剪切、复制、重命名、删除等常用操作不依赖扩展菜单。
选择“更多选项”后，日志记录 `context_menu more_options requested`，再加载完整系统菜单；
第三方扩展仍可能在此阶段阻塞。侧栏系统位置菜单仍沿用 Shell 路径。
排查时对比 `query_context_menu`、`read_menu_tree` 等阶段的开始、结束与 `elapsed_ms`，
不要仅凭菜单中出现某个软件名称就认定它是卡顿来源。

- **GUI 文件日志开关需重启。** 启动时先读取 `diagnostics.runtime_log`，再初始化日志；
  配置创建和解析错误在初始化后报告。配置无效时回退到默认关闭，过滤级别及保留数量沿用默认值。
- **`time.busy` は文字列。** `"12.2ms"` / `"1.5s"` / `"696µs"` と整形済みで、数値ではありません。
  集計する側が単位を解釈する必要があります
- **集計が無い。** 生のレコードが読めるだけで、「合計時間の多い順」は `noh perf` の仕事です
- **パスと検索語がそのまま残る。** 意図的な設計と、その代わりに何を境界にしているかは §1.1 に書きました
- **モードを締めるのは Unix だけ。** Windows は実行ファイルのディレクトリの ACL を継承します。
  共有ディレクトリに配置する場合は、ログへのアクセス権限にも注意してください。
