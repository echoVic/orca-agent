# Orca

ターミナル向けの DeepSeek ネイティブなコーディングエージェントです。

Orca にタスクを渡すと、コードの読み取り、ファイルの編集、コマンドの実行、
結果の検証を行い、完了するか判断が必要になるまで作業を続けます。
対話的な作業には TUI、スクリプトや CI には `orca exec` を使用できます。
Orca は Rust 製で、ローカルで動作し、MIT ライセンスで提供されます。

[English](README.md) · [简体中文](README.zh-CN.md) · [日本語](README.ja-JP.md) · [Tiếng Việt](README.vi.md) · [한국어](README.ko-KR.md) · [Español](README.es-419.md) · [Português](README.pt-BR.md)

[Web サイト](https://orcaagent.dev/) · [変更履歴](https://orcaagent.dev/changelog/) · [リリース](https://github.com/echoVic/orca-agent/releases/latest) · [npm](https://www.npmjs.com/package/@blade-ai/orca)

## インストール

```bash
npm install -g @blade-ai/orca
```

ネイティブバイナリを直接インストールすることもできます。

```bash
curl -fsSL https://orcaagent.dev/install.sh | sh
```

Windows PowerShell の場合：

```powershell
irm https://orcaagent.dev/install.ps1 | iex
```

プロジェクトディレクトリで、制限付きサンドボックスを構成します。

```powershell
& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox
```

npm パッケージは macOS、Linux、Windows の ARM64 / x64 に対応しています。
ビルド済みアーカイブは [GitHub Releases](https://github.com/echoVic/orca-agent/releases/latest) からも入手できます。

## 使い方

```bash
export DEEPSEEK_API_KEY=sk-...

orca                                      # TUI を開く
orca exec "失敗しているテストを修正"        # ヘッドレスで実行
orca exec --verifier "cargo test" "修正する" # 完了前に検証
orca --mode=acp                           # ACP クライアントを接続
```

Windows PowerShell では `$env:DEEPSEEK_API_KEY = "sk-..."` でキーを設定します。
以降の `orca` コマンドは同じです。

TUI では `@` でファイル、Skills、Plugins、MCP Resources を検索できます。
`/plan` は読み取り専用の計画、`/goal` は永続的な目標、`/tasks` はバックグラウンド作業を
表示するタスク dock、`/agents` は Agent Workspace、`/recap` はセッションの要約です。
承認が必要なツール呼び出しは入力欄の位置に表示され、`Shift+Tab` で承認モードを切り替えます。
`/trust` はプロジェクトの設定と指示を読み込むかどうかを決めるもので、OS サンドボックスは変更しません。
画面とキー操作の詳細は [Terminal UI ガイド](https://orcaagent.dev/docs/#terminal-ui)（英語・中国語）を参照してください。

### Pilion Browser から Orca を使う

[Pilion Browser](https://github.com/echoVic/pilion-browser) は ACP クライアントとして動作するデスクトップブラウザーです。Agent パネルで **Orca** を選ぶと、Pilion は `orca --mode=acp` を起動し、`DEEPSEEK_API_KEY` を転送し、自身のタブを MCP ツール（`browser_snapshot`、`browser_screenshot`、ナビゲート、クリック、入力）として Orca に公開します。操作前の承認と人間による引き継ぎに対応しています。macOS、Windows、Linux のインストーラーは [Pilion のリリースページ](https://github.com/echoVic/pilion-browser/releases) にあります。

## 主な機能

- DeepSeek の推論とツール利用のセマンティクスに直接対応し、SSE ストリーミング、
  プレフィックスキャッシュに適したプロンプト、自動コンテキスト管理、再試行を提供します。
- コードの読み取り、検索、編集、書き込み、シェルコマンドの実行、指定コマンドでの検証を行います。
- `suggest`、サンドボックス内の `auto-edit`、フルアクセスの `full-auto`、
  読み取り専用の `plan` とフォルダー単位の信頼設定でリスクを制御します。
- ローカルの会話履歴を保存し、再開、フォーク、検索、名前変更、アーカイブ、圧縮に対応します。
- 固定ターン上限のない永続的な目標、サブエージェント、JavaScript ワークフローで長時間のタスクを処理します。
- 信頼済みワークスペースから指示、Skills、Plugins、カスタムツール、MCP ツールとリソースを読み込みます。
- エディター、ハーネス、CI 向けに安定した JSONL、app-server、Agent Client
  Protocol（ACP）の契約を提供します。

設定の優先順位は、環境変数、CLI 引数、設定ファイル、既定値です。
完全なコマンド一覧は `orca --help` または `orca exec --help` で確認できます。
ユーザー設定は `~/.orca/config.toml` にあり、信頼済みプロジェクトでは
`.orca/config.toml`、`AGENTS.md`、ルール、Skills、ワークフローも利用できます。

詳細:

- [Persistent Goal Mode](docs/goal-mode.md)
- [Harness と app-server の契約](docs/harness-contract.md)
- [動的ワークフロー設計](docs/claude-code-workflow-parity.md)
- [プロダクションロードマップ](docs/production-roadmap.md)

## コミュニティ

- QQ グループ: `472309526`
- [Telegram](https://t.me/+11No1w5ZbTMyZTQ1)

## コントリビューション

コントリビューションの前に [CONTRIBUTING.md](CONTRIBUTING.md) をお読みください。
大規模または互換性に影響する変更は、先に Issue を作成してください。

- [バグを報告](https://github.com/echoVic/orca-agent/issues/new?template=bug_report.yml)
- [機能を提案](https://github.com/echoVic/orca-agent/issues/new?template=feature_request.yml)
- [サポートを受ける](SUPPORT.md)
- [脆弱性を報告](SECURITY.md)

## ライセンス

[MIT](LICENSE)
