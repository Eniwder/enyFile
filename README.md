# enyFile

[![CI](https://github.com/Eniwder/enyFile/actions/workflows/ci.yml/badge.svg)](https://github.com/Eniwder/enyFile/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Eniwder/enyFile)](https://github.com/Eniwder/enyFile/releases/latest)

Windows 専用の、キーボード中心の 2 ペインファイラー。Rust + [Slint](https://slint.dev/) 製。

## NexusFile へのリスペクト

enyFile は、長く愛用してきたファイラー **NexusFile** をリスペクトして作っています。画面構成や操作感は NexusFile を強く参考にしています。

enyFile は NexusFile とは無関係の個人プロジェクトで、NexusFile のコードや画像などは使っていません。enyFile についての質問や不具合報告は、NexusFile の作者ではなく、このリポジトリの [Issues](https://github.com/Eniwder/enyFile/issues) または [@Eniel120](https://x.com/Eniel120) へお願いします。

## 特徴

- 2 / 3 / 4 ペイン
- スタイリッシュなデザイン
  - 配色・フォント・ブックマークなどのカスタマイズ
- 高速な動作

## 動作環境

- Windows 10 / 11（x64）

## インストール

[Releases](https://github.com/Eniwder/enyFile/releases/latest) から `enyFile-vX.Y.Z-windows-x64.zip` をダウンロードして展開し、`enyfile.exe` を実行します。インストーラーはありません。

```powershell
enyfile.exe                       # 起動時のフォルダー（設定）を開く。未設定なら左: デスクトップ / 右: ドキュメント
enyfile.exe "D:\Work" "E:\Backup" # 左右のフォルダーを指定
```

設定は `%APPDATA%\enyFile\config.json` に保存されます。

## 基本操作

| キー | 動作 |
| --- | --- |
| Ctrl+C / Ctrl+X / Ctrl+V | コピー / 切り取り / 貼り付け |
| Delete / Shift+Delete | ごみ箱へ / 完全削除 |
| Enter / Backspace | 開く / 親フォルダーへ |
| Tab | ペインの切り替え |
| F2 | リネーム |

並べ替え、ペイン数の切り替え、設定画面など、そのほかの操作と画面の説明は [app/README.md](app/README.md) を参照してください。

## ソースからビルド

[Rust](https://rustup.rs/)（stable, MSVC ツールチェーン）と Visual Studio Build Tools が必要です。

```powershell
git clone https://github.com/Eniwder/enyFile.git
cd enyFile
cargo build --release --manifest-path app/Cargo.toml
.\enyFile.cmd    # app\target\release\enyfile.exe を起動
```

`app\target` から起動した開発ビルドは、設定を `app\data\config.json` に保存し、起動フォルダー未設定時は試験用の `app\data\sandbox` を開きます（実ファイルを触らないため）。

テスト:

```powershell
cargo test --release --manifest-path app/Cargo.toml   # 単体テスト
.\app\Check.ps1                                       # ビルド＋単体テスト＋Explorer 連携スモーク（クリップボードを書き換えます）
```

## リリース

`app/Cargo.toml` の `version` を上げてコミットし、同じ番号のタグを push すると GitHub Actions がビルドして Release を作成します。

```powershell
git tag v0.1.0
git push origin v0.1.0
```

| ワークフロー | タイミング | 内容 |
| --- | --- | --- |
| [CI](.github/workflows/ci.yml) | main への push / PR | ビルドと単体テスト |
| [Release](.github/workflows/release.yml) | `v*` タグの push | バージョン照合・テスト・ビルド後、zip を Release に添付 |

## 構成

```text
app/
├── Cargo.toml
├── Check.ps1           ビルド＋テスト＋スモーク
└── src/
    ├── main.rs         UI スレッドの制御、ペインごとの一覧ワーカー
    ├── ui.slint        画面・ダイアログ・テーマ
    ├── listing.rs      列挙・自然順ソート・選択モデル
    ├── paste.rs        貼り付けの計画（衝突の質問、再帰結合、別名）
    ├── shell.rs        クリップボード、IFileOperation、フォルダー監視
    ├── ctxmenu.rs      シェルのコンテキストメニュー
    ├── icons.rs        関連付けアイコン
    ├── config.rs       設定ファイル
    ├── theme.rs        配色
    ├── smoke.rs        自動スモークテスト
    └── bin/shellprobe.rs  スモーク用の Explorer 役
```

## Pull Request について

- **バグ修正**：歓迎します。
- **機能追加・機能改善**：作者の好みに合うものだけを受け入れます。せっかく作っていただいても見送ることがあるので、大きな変更の前には Issue で相談してもらえると助かります。

## ライセンス

自作のソースコードは [MIT License](LICENSE) です。

ただし UI ツールキットの Slint を GPLv3 で利用しているため、ビルドしたアプリケーション（Release の配布物を含む）は **GPL-3.0-only** の条件で配布します（全文は [COPYING](COPYING)、`app/Cargo.toml` の `license` も同じ）。各 Release に対応するソースコードは、同じ名前のタグで入手できます。第三者の依存ライブラリはそれぞれのライセンスに従います。
