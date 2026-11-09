# enyFile 開発者向け

ビルド・テスト・リリースと、ソースの構成。使い方はルートの [README](../README.md) と [マニュアル](../docs/MANUAL.md) を参照。

## ビルド・起動

[Rust](https://rustup.rs/)（stable, MSVC ツールチェーン）と Visual Studio Build Tools が必要。

```powershell
git clone https://github.com/Eniwder/enyFile.git
cd enyFile
cargo build --release --manifest-path app/Cargo.toml
.\enyFile.cmd    # app\target\release\enyfile.exe を起動
```

`app\target` から起動した開発ビルドは、設定を `app\data\config.json` に保存する。引数なしで起動フォルダも未設定なら、試験用フォルダ `app\data\sandbox\left` と `right` を開く（実ファイルを触らないため。初回はサンプルファイル付きで作成）。別のフォルダは `enyFile.cmd "左のパス" "右のパス"` で指定する。

## テスト

```powershell
cargo test --release --manifest-path app/Cargo.toml   # 単体テスト
.\app\Check.ps1                                       # ビルド＋単体テスト＋Explorer 連携スモーク（クリップボードを書き換える）
.\app\Check.ps1 -NoBuild                              # 再ビルドを省略
```

`Check.ps1` はフォルダ内に `.tools` があればそれを使い（オフラインでビルド）、なければ PATH 上の Rust を使う。スモークのデータは `app/data/` に作成し、次回実行時に削除する。

README のアニメーション（`docs/demo.webp`）は `.\app\Demo.ps1` で作り直せる（ffmpeg が必要）。決まった内容の試験フォルダを毎回作り、`app/src/demo.rs` の台本どおりにキーを送って 1 場面ずつ撮るので、何度作っても同じ映像になる。機能を足したら台本に場面を加えて実行し直す。録画中の約 1 分はウィンドウに触れないこと。

## リリース

`app/Cargo.toml` の `version` を上げ、`Cargo.lock` も一緒にコミットしてから、そのコミットに同じ番号のタグを付けて push すると、GitHub Actions がビルドして Release を作成する。

```powershell
# app/Cargo.toml の version を 0.1.1 に変更してから
cargo update -p enyfile --manifest-path app/Cargo.toml   # Cargo.lock のバージョンを合わせる
git commit -am "v0.1.1"
git push
git tag v0.1.1      # バージョンを上げたコミットに付ける
git push origin v0.1.1
```

| ワークフロー | タイミング | 内容 |
| --- | --- | --- |
| [CI](../.github/workflows/ci.yml) | main への push / PR | ビルドと単体テスト、依存クレートの既知の脆弱性・ライセンス・取得元の確認（cargo-deny、設定は `app/deny.toml`） |
| [Release](../.github/workflows/release.yml) | `v*` タグの push | バージョン照合・テスト・ビルド後、zip と SHA-256 を Release に添付し、zip と exe のビルド来歴（attestation）を記録 |

## 構成

```text
app/
├── Cargo.toml
├── Check.ps1           ビルド＋テスト＋スモーク
├── Demo.ps1            README のアニメーションを録画（docs/demo.webp）
└── src/
    ├── main.rs         UI スレッドの制御、ペインごとの一覧ワーカー（結果は invoke_from_event_loop で受け取る）
    ├── ui.slint        画面・ダイアログ・テーマ
    ├── rows.rs         ペインの一覧のモデル（".."・項目・ドライブ、ページ送り）
    ├── settings.rs     設定ダイアログ・配色・フォント・ブックマーク・設定の保存
    ├── listing.rs      列挙・自然順ソート・選択モデル
    ├── paste.rs        貼り付けの計画（1 件ずつの質問、再帰結合、別名と検証）
    ├── pathid.rs       パスの同一性の判定（ファイル ID、リンクを辿る）
    ├── props.rs        プロパティ画面（Zone.Identifier、代替データストリーム、CNG での SHA-256）
    ├── finder.rs       フォルダ以下の検索
    ├── compare.rs      ペインの比較
    ├── agent.rs        エージェント CLI の起動・中断（ジョブオブジェクト）・JSONL の読み取り。CLI ごとの違いは mod codex
    ├── shell.rs        クリップボード、IFileOperation、既定アプリ起動、フォルダ監視、DLL の検索先の制限
    ├── dnd.rs          OLEのドロップ受付、宛先判定、ホイールと描画の連携
    ├── dnd/smoke.rs    実UI・OLEの回帰テスト（releaseの --smoke でも使用）
    ├── dnd/tests.rs    D&Dの単体テスト
    ├── ctxmenu.rs      シェルのコンテキストメニュー
    ├── icons.rs        関連付けアイコン
    ├── config.rs       設定ファイル
    ├── theme.rs        配色
    ├── smoke.rs        自動スモークテスト
    ├── demo.rs         README のアニメーションの台本と録画（--demo）
    └── bin/shellprobe.rs  スモーク用の Explorer 役
```

## 手動確認表（未実施）

### 実環境のネットワークフォルダ検証

`enyfile.exe --network-smoke <ローカルの検証用ディレクトリ> <リモートの検証用フォルダ>` で、専用の設定・ログを使うテストインスタンスを起動できる。ローカル側の `local` サブフォルダとリモート側の検証用フォルダは先に作成する。このモード自体はWSLを停止・起動しない。

ローカルの検証用ディレクトリに `command.json`（例: `{"id":1,"action":"refresh"}`）を書き込み、`status.json` の `ack` が同じIDになるまで待つ。IDは増加させる。操作は `refresh`（右ペイン更新）、`automatic`（自動更新）、`resume`（復帰処理）、`local` / `remote`（右ペイン移動）、`slow`（リモートの `slow` サブフォルダへ移動）、`probe`（メニュー・Tab・左ペイン更新）、`exit`。`cached-remote` はローカルの一覧を使ってリモートの最終表示を再現するテスト用データ、`stop-watch` は監視の消失を再現する操作。状態にはUIタイマーの実行回数、一覧、待機状態、監視スレッドの生存状態、更新世代、操作結果を含む。WSL停止・起動を外部から行い、停止中もタイマーとUI操作が進むこと、再接続後に一覧を読み直せることを確認する。

### スリープ復帰・ネットワーク切断

スリープ復帰・ネットワーク切断時の確認:

- Windowsのユーザー復帰通知でフォルダ監視を張り直し、表示中の一覧を更新して再描画を要求する。
- アクティブ化・復帰・監視通知などによる自動更新では、WSLの稼働状態を別スレッドで確認する。停止中、または状態を確認できない場合は前の一覧を残し、WSLを起動せずに更新を見送る。F5やフォルダを開く操作では明示的にアクセスする。
- 監視スレッドが終了・失敗した場合は生存状態を解除する。次回の一覧取得成功時に張り直す。WSLは通知が届かないまま監視が残る場合もあるため、一覧の再取得成功時に監視を作り直す。アクティブ化時の再読み込みは引き続き変更通知の補完として使い、定期ポーリングは行わない。
- フォルダの一覧取得は20秒で待機状態を解除し、直前の一覧と履歴を保持する。F5で再試行できる。
- タイムアウト時はWindowsへ読み取りスレッドのI/O中断を要求する。ドライバが中断に対応していない場合、そのスレッドの復帰までは同じペインの再読み込みが成功しない場合がある。
- 設定ファイルと同じ場所の `recovery.log` に一覧取得のタイムアウト、15秒以上のUI応答の途切れ、応答再開、1分ごとの稼働状態を記録する。ログ出力は別スレッド。スリープ自体でも応答は途切れるため、途切れだけでフリーズとは断定しない。
- ログには表示フォルダのパスを含む。256 KiBを超えると `recovery.previous.log` に退避する。
- [x] 実機のWSLフォルダを片方のペインで開き、スリープ・復帰後に両ペイン、メニュー、F5、ローカルフォルダへの移動を確認する。

実際のExplorerウィンドウと、通常権限で確認する。

- [x] アプリでCtrl+C → ExplorerでCtrl+V。複数・日本語名・フォルダがコピーされ、元は残る
- [x] アプリでCtrl+X → ExplorerでCtrl+V。移動し、アプリの一覧から自動で消える
- [x] ExplorerでCtrl+C → アプリでCtrl+V。貼り付けた項目が選択状態になる
- [x] ExplorerでCtrl+X → アプリでCtrl+V。Explorer側の半透明表示が解除され、2回目のCtrl+Vでは何も起きない
- [x] 同名衝突で自作の確認画面が出る。別名・上書き・スキップ・一括・新しい場合のみ・名前の検証が意図どおり。Windowsの確認画面は出ない
- [x] 同じフォルダへのCtrl+Vで「- コピー」ができる
- [x] 大きなファイルのコピー中もUIが動き、Windowsの進行状況ダイアログでキャンセルできる
- [x] Delete（ごみ箱）とShift+Delete、Explorerの「元に戻す」との関係
- [x] Explorerからのファイル・フォルダのドロップを受け付ける（基本の実マウス操作を確認済み）
- [x] ペイン間D&D・同じペイン内のフォルダへの移動の設定、宛先の列判定、ホイールと描画は自動スモークで確認。詳細な範囲と未確認項目は [D&D実装と検証](../docs/DND.md)
- [x] アプリからブラウザのアップロード欄・Explorer・デスクトップへドラッグ。複数・検索結果・Shift/Ctrl でのコピー・移動の切り替え、終わったあとクリックが普通に効く
- [x] ネットワークフォルダ、アクセス拒否のフォルダ、長いパス

### 1件ずつ問う確認画面（未実施）

- [x] ファイルの衝突で、上書き・新しければ上書き・スキップ・リネーム・キャンセルが意図どおり。括弧内の文字キーでも選べる
- [x] フォルダの結合で、中の衝突も1件ずつ問われる。「すべて適用」はファイルとフォルダで別々に効く
- [x] フォルダの置換で、既存フォルダが削除されて置き換わる
- [x] リネームで拡張子を除いた部分が選択され、重複名や使えない文字が拒否される
- [x] 切り取り→結合で移動したあと、空になった移動元フォルダが消え、スキップしたものは残る
- [x] 途中でキャンセルすると、ファイルが何も変わらない
