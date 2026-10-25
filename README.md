# enyFile

[![CI](https://github.com/Eniwder/enyFile/actions/workflows/ci.yml/badge.svg)](https://github.com/Eniwder/enyFile/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Eniwder/enyFile)](https://github.com/Eniwder/enyFile/releases/latest)

Windows 専用、キーボード中心の 2 ペインファイラー。Rust + [Slint](https://slint.dev/) 製。

![enyFile のデモ: 2 ペインの操作、フィルター、左右の比較、4 ペイン、エージェントへの依頼、ライト配色と游ゴシックへの切り替え](docs/demo.webp)

## 特徴

- 2 / 3 / 4 ペイン
- 軽快な動作とスタイリッシュなデザイン
- シンプルな操作と実用的な機能
  - フィルター、フォルダ以下の検索、ペイン間の比較 など
- 安全を重視した小さく閉じた設計
  - 直接依存は 6 ライブラリ（Slint, windows-rs など）
  - 本体のインターネット通信なし（更新確認やテレメトリなし）
- AI エージェント連携（Codex CLI）
  - 外部プロセスとして実行。enyFile は API キーを保持しない
  - サンドボックス内でコマンド実行時の承認なしに実行。既定の書き込み範囲はアクティブペインのフォルダ以下

## ダウンロード

[Releases](https://github.com/Eniwder/enyFile/releases/latest) から `enyFile-vX.Y.Z-windows-x64.zip` をダウンロードして展開し、`enyfile.exe` を実行します。

- 動作環境: Windows 10 / 11（x64）
- 設定は `enyfile.exe` と同じフォルダの `config.json` に保存されます（ポータブル）。書き込める場所に展開してください
- コード署名がないため、初回は SmartScreen の警告が出ることがあります。「詳細情報」→「実行」で起動できます

更新の方法や、ダウンロードしたファイルの検証方法は [マニュアル](docs/MANUAL.md#インストールと更新) を参照してください。

## 基本操作

| キー | 動作 |
| --- | --- |
| Ctrl+C / Ctrl+X / Ctrl+V | コピー / 切り取り / 貼り付け（Explorer とも相互に使える） |
| Delete / Shift+Delete | ごみ箱へ / 完全削除 |
| Enter / Backspace | 開く / 親フォルダへ |
| Tab | ペインの切り替え |
| F2 | リネーム |
| Ctrl+F / Ctrl+Shift+F | 検索 / フィルター |
| Ctrl+I | エージェントに文章で依頼 |

詳しい操作方法は [マニュアル](docs/MANUAL.md) を参照してください。

## エージェント（Codex CLI 連携）

手作業では面倒なファイル操作を、文章で頼めます（例:「選択した画像を 256×256 に縮小して」「この写真を撮影日のフォルダに分けて」）。考えてコマンドを実行するのは [Codex CLI](https://github.com/openai/codex) です。

1. Codex CLI をインストールしてサインインしておく（`npm install -g @openai/codex` → `codex login`）
2. **Ctrl+I** でパネルを開き、依頼を入力して Enter

渡す情報や制限の詳細は [マニュアル](docs/MANUAL.md#エージェント) を参照してください。

## 開発

ソースからのビルド、テスト、リリースの手順は [開発者向け README](app/README.md) を参照してください。

Pull Request について:

- **バグ修正**：歓迎します。
- **機能追加・機能改善**：作者の好みに合うものだけを受け入れます。せっかく作っていただいても見送ることがあるので、大きな変更の前には Issue で相談してもらえると助かります。

## NexusFile へのリスペクト

enyFile は、長く愛用してきたファイラー **NexusFile** をリスペクトして作っています。画面構成や操作感は NexusFile を強く参考にしています。

enyFile についての質問や不具合報告はこのリポジトリの [Issues](https://github.com/Eniwder/enyFile/issues) または [@Eniel120](https://x.com/Eniel120) へお願いします。

## ライセンス

自作のソースコードは [MIT License](LICENSE) です。

ただし UI ツールキットの Slint を GPLv3 で利用しているため、ビルドしたアプリケーション（Release の配布物を含む）は **GPL-3.0-only** の条件で配布します（全文は [COPYING](COPYING)、`app/Cargo.toml` の `license` も同じ）。各 Release に対応するソースコードは、同じ名前のタグで入手できます。第三者の依存ライブラリはそれぞれのライセンスに従います。
