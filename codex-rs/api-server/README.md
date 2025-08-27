# Codex API Server

最小限のHTTP APIサーバー実装。既存の`codex-core`の`ModelClient`を再利用し、ChatGPT/OpenAI APIへのアクセスをREST APIとして公開します。

## 概要

- **フレームワーク**: Axum (Tokio ベース)
- **認証**: 既存の `auth.json` (`CODEX_HOME/auth.json`) を使用
- **モデル通信**: `codex-core::ModelClient` を直接利用（Responses API / Chat Completions API 自動切替）

## エンドポイント

### `GET /healthz`
ヘルスチェック用エンドポイント。

**レスポンス**: `200 OK` with body `"ok"`

### `POST /chat`
非ストリーミングチャット。最終的なアシスタントメッセージをJSONで返します。

**リクエスト例**:
```json
{
  "messages": [
    {"role": "user", "content": "Hello, how are you?"}
  ],
  "model": "gpt-5",  // オプション（デフォルトは設定ファイルの値）
  "stream": false    // オプション（このエンドポイントでは無視）
}
```

**レスポンス例**:
```json
{
  "message": {
    "role": "assistant",
    "content": "I'm doing well, thank you! How can I help you today?"
  }
}
```

### `POST /chat/stream`
ストリーミングチャット（Server-Sent Events）。リアルタイムでレスポンスを返します。

**リクエスト**: `/chat` と同じ形式

**レスポンス**: Server-Sent Events ストリーム
- `event: delta` - テキストの差分
- `event: reasoning` - 推論内容（GPT-5系モデルのみ）
- `event: reasoning_summary` - 推論サマリー
- `event: item` - 完成したアイテム（メッセージ、ツール呼び出しなど）
- `event: completed` - ストリーム完了
- `event: error` - エラー発生時

## ビルド・実行方法

### ビルド
```bash
cd codex-rs
cargo build -p codex-api-server --release
```

### 実行
```bash
# デフォルト設定で起動（ポート8080）
cargo run -p codex-api-server

# 環境変数でカスタマイズ
PORT=3000 cargo run -p codex-api-server

# ビルド済みバイナリを直接実行
./target/release/codex-api-server
```

### 環境変数
- `PORT` - APIサーバーのポート番号（デフォルト: 8080）
- `CODEX_HOME` - Codex設定ディレクトリ（デフォルト: `~/.codex`）
- `RUST_LOG` - ログレベル（例: `info`, `debug`, `trace`）

### 事前準備
1. `codex login` を実行して認証情報を設定済みであること
2. または `OPENAI_API_KEY` 環境変数が設定されていること

## 実装の詳細（Deep Dive）

このAPIサーバーの `/chat` と `/chat/stream` は、既存の `codex-rs` のストリーミング実装をそのまま薄いHTTP層で包んだものです。下記に、どのコードからロジックを利用しているか、どのような仕組みか、そして Codex 本来の「エージェントフロー」をどこまで再現しているかをまとめます。

### どのコードを使っているか（ソース由来）
- `codex_core::ModelClient`（`core/src/client.rs`）
  - `ModelClient::stream(&Prompt)` が中核。プロバイダの `wire_api` に応じて、
    - Responses API 経由: `ModelClient::stream_responses()` に委譲
    - Chat Completions API 経由: `chat_completions::stream_chat_completions()` に委譲
- Responses API 実装: `core/src/client.rs` 内 `stream_responses()`
  - `ResponsesApiRequest`（`core/src/client_common.rs`）を構築
  - 認証ヘッダ付与、`originator`/`User-Agent` ヘッダ、`chatgpt-account-id` ヘッダ（ChatGPTアカウント時）
  - SSE を `process_sse` で `ResponseEvent`（`Created`/`OutputItemDone`/`Completed` など）へ変換
- Chat Completions 実装: `core/src/chat_completions.rs` の `stream_chat_completions()`
  - 従来の `messages[]` 形式のJSONを組み立て、SSEを `process_chat_sse` で `ResponseEvent` にマッピング
  - ツール呼び出し（function_call）のストリーミング片を終端時に `FunctionCall` として統合
- 認証管理: `codex-login`
  - `AuthManager`（`login/src/auth_manager.rs`）と `CodexAuth` で `auth.json`/環境変数を読み込み、必要に応じてトークンを自動リフレッシュ
- URL/ヘッダ構築: `core/src/model_provider_info.rs`
  - `ModelProviderInfo::create_request_builder()` が Bearer 認証付与・任意ヘッダ挿入を実施

### APIサーバー内でのブリッジ方法
- `/chat`（非ストリーミング）
  1. リクエストの `messages` を `codex_protocol::models::ResponseItem::Message` に変換し、`Prompt` に積む
  2. `ModelClient` を生成（`Config`, `AuthManager`, `ModelProviderInfo`, `session_id` 等を注入）
  3. `ModelClient::stream_owned(prompt)` を呼び、`ResponseEvent` ストリームを取得
  4. ストリームから `ResponseEvent::OutputItemDone(Message{role:"assistant"...})` を収束して最終テキストを抽出し、JSONで返す

- `/chat/stream`（SSE）
  1. 上と同様に `Prompt` と `ModelClient` を構築
  2. ストリームの各 `ResponseEvent` を SSE `Event` にマッピングし、そのままクライアントへ送出
     - `OutputTextDelta` → `event: delta`
     - `ReasoningContentDelta` → `event: reasoning`
     - `ReasoningSummaryDelta` → `event: reasoning_summary`
     - `OutputItemDone`（Message/FunctionCall など） → `event: item`
     - `Completed` → `event: completed`
     - エラー → `event: error`

### 仕組み（リトライ/ヘッダ/プラン分岐/アカウントヘッダ）
- リトライ・タイムアウト: `core/src/client.rs` および `chat_completions.rs` 側で、HTTP失敗時のリトライ・SSEアイドルタイムアウト・`Retry-After` などをハンドリング
- ヘッダ付与:
  - `Authorization: Bearer <token>` は `ModelProviderInfo::create_request_builder()` が付与
  - Responses API 時は `OpenAI-Beta: responses=experimental`, `session_id`, `originator`, `User-Agent`
  - ChatGPT アカウント認証時は `chatgpt-account-id`（`core/src/client.rs`）
- プロバイダ分岐: `Config.model_provider` と `ModelProviderInfo.wire_api`（Responses/Chat）で実行経路を切替
- ツール/推論（GPT-5系）:
  - `Prompt.tools` に `OpenAiTool` を積み、各ワイヤプロトコル用のJSONに変換
  - GPT-5系では `reasoning`（`effort`/`summary`）や `text.verbosity` を適用

### 「Codex本来の仕組み（エージェントフロー）」は再現できているか？
- 本APIサーバーは「モデルへの問い合わせ層（推論呼び出し）」をそのまま提供しています。
- つまり、以下は「既存 `codex-core` と同等」に機能します：
  - モデルとのストリーミング通信
  - ツール呼び出し（function_call等）のストリーミングの取り扱いと終端時の統合
  - GPT-5系の推論データ（reasoning delta 等）取り扱い
  - 認証管理（ChatGPT アカウント/OPENAI_API_KEY）・自動リフレッシュ・必要ヘッダの付与
  - リトライ/タイムアウト/`Retry-After`/エラー本文の抽出
- 一方、以下の「Codexのアプリ層（エージェントの履歴管理やMCPサーバー協調、TUIのUXなど）」は含めていません：
  - 会話履歴の永続化/要約/適切なコンテキスト管理（`conversation_history.rs` や `conversation_manager.rs` 相当）
  - MCPサーバーとの外部ツール連携（`mcp-client`/`mcp-servers` を介した拡張ツール呼び出しと合流）
  - TUIの表示・ステータス管理・スナップショットテストに紐づくUIレイヤ
  - apply_patch 等の特殊UXを前提としたCLI/TUI統合（コマンド実行承認フロー等）

> 結論: 「モデル推論ストリーミング」「ツール呼出のイベント処理」「認証・リトライ等のネットワーク制御」は本APIで再現済み。プロジェクト全体の高度なエージェント・オーケストレーション（履歴/MCP/TUI/ポリシー判断等）は対象外（今後拡張可能な境界）。

### 実装の注意点（本API内）
- `ResponseStream` は `codex-core` 内部型のため、API側では `ModelClient::stream_owned(prompt)` で所有権移譲し、`impl Stream<Item = Result<ResponseEvent>>` として扱うように設計
- `Arc<ModelClient>` を使い、Rust 2024 の `impl Trait` ライフタイム捕獲問題を回避
- `Prompt` は `codex_protocol::models::ResponseItem` で作成（`Message { role, content }`）
- `/chat` では最終 `OutputItemDone(Message: assistant)` を収束し返却（`collect_final_message`）
- `/chat/stream` では `ResponseEvent` をSSE `Event` へ直接map

## 今後の拡張案
- 認証ミドルウェア（APIキー/Bearer）
- レート制御・メトリクス・監視
- WebSocket サポート
- マルチテナント（複数の `CODEX_HOME` 切替）
- 会話履歴永続化と要約、プロジェクト文脈の自動差し込み
- MCP連携のAPI化（カスタム外部ツール呼び出し）

## 現状の限界と codex-cli との機能差分（考察）

ご指摘のとおり、「単純な POST/GET と SSE だけ」では codex-cli の全機能（とくにコマンド実行や複合的なエージェント挙動）を完全には再現できていません。本APIは「モデル推論ストリーミング層」を提供するもので、以下の主要なギャップが残っています。

### 1) コマンド実行（ローカル/サンドボックス）
- 現状: `/chat`/`/chat/stream` は `ResponseEvent` をそのまま返すだけで、ツール呼び出し（例: function_call → shell/exec）の「実行」は行いません。`event: item` で FunctionCall などは流れてきますが、サーバー側でコマンドを実際に実行して結果を `FunctionCallOutput` として戻す経路は未実装です。
- codex-rs の実装箇所:
  - 実行系: `core/src/exec_command/*`, `core/src/exec_env.rs`, `core/src/shell.rs`, `core/src/exec.rs`
  - サンドボックス/ポリシー: `core/src/seatbelt.rs`, `core/src/execpolicy`, `core/src/landlock.rs`
- 理由: コマンド実行は副作用のある操作であり、権限・ポリシー・I/O・ストリーミングの双方向経路・安全な隔離（Seatbelt / Landlock 等）を伴うため、単純なリクエスト/レスポンス API だけでは足りません。本APIはそこへのブリッジをまだ設けていません。

### 2) MCP サーバー連携（外部ツール）
- 現状: モデルが `web_search` などの外部ツールを使おうとするイベントは `ResponseEvent` に現れ得ますが、MCP クライアントを通じて外部ツール呼び出し・結果を返すループは未実装です。
- codex-rs の実装箇所: `mcp-client`, `core/src/mcp_connection_manager.rs`, `core/src/mcp_tool_call.rs`
- 理由: MCP は別プロセス/サーバーとのストリーミング連携が必要で、結果をモデル側にフィードバックする仲介（ツールブローカ）が必要になります。

### 3) エージェントの会話履歴管理・要約・プロジェクト文脈の注入
- 現状: 本APIは渡された `messages` を 1 リクエスト内で組み立てて送るだけで、継続的な会話履歴管理（永続化・トークン制約に合わせた要約・プロジェクトドキュメントの自動差し込み等）は未実装。
- codex-rs の実装箇所: `core/src/conversation_history.rs`, `core/src/conversation_manager.rs`, `core/src/project_doc.rs`
- 理由: サーバー側で履歴を持ち、要約・圧縮・ドキュメント挿入を行う「状態管理」レイヤが必要です。

### 4) 承認フロー（Approval Policy）と実行ポリシー
- 現状: コマンド実行前の承認ダイアログや、信頼レベル（trusted/untrusted）に応じた自動/手動承認は未実装。
- codex-rs の実装箇所: `core/src/config.rs` の `approval_policy`, `core/src/execpolicy`, `core/src/is_safe_command.rs`
- 理由: 人間の承認を介在させるUI/ワークフローが必要で、HTTP API では別エンドポイント・通知/キューイングが求められます。

### 5) apply_patch（編集適用）等の特殊ツール UX
- 現状: モデルが `apply_patch` ツールを使う前提の合意/差分検証/適用といった対話的プロセスは未実装です。
- codex-rs の実装箇所: `core/src/apply_patch.rs`, `core/src/openai_tools.rs`
- 理由: 実ファイルへの安全な適用・差分の提示・ロールバック等を伴うため、追加のAPI設計が必要です。

### 6) TUI/CLI のUX（ストリーミングの集約・状態表示・スナップショット）
- 現状: TUI/CLI の洗練されたUX（ストリーム集約表示、入力補助、進捗/ステータス、スナップショットテスト連携等）は本APIには含まれません。
- codex-rs の実装箇所: `tui/`, `core/src/client.rs` の `AggregateStreamExt`（集約ロジック）

### 7) セキュアなサンドボックス・Seatbelt の直接利用
- 現状: APIサーバーは Seatbelt/landlock の起動/制御は行っていません。
- codex-rs の実装箇所: `core/src/seatbelt.rs`, `core/src/landlock.rs`, `linux-sandbox/`
- 理由: コマンド実行の前提となる安全性要件で、OS別準備・権限設定・プロセス分離が必要です。

---

## では、どう拡張すれば codex-cli に近づくか（非実装、方針のみ）

本READMEでは実装は行いませんが、codex-cli の再現には以下のような「ブローカ/オーケストレータ」層が必要です。

1. ツール実行ブローカ（サーバー側）
   - `ResponseEvent::OutputItemDone(FunctionCall { .. })` を検知
   - 種別に応じて実行（例: shell/exec → `exec_command/*` を呼ぶ、MCP → `mcp-client` 経由）
   - 実行結果を `FunctionCallOutput` としてモデルに差し戻す（Responses/Chat のプロトコルに従い、継続ターンを生成）
   - 双方向のSSE/WebSocketで長生きさせ、複数ターンを橋渡しする

2. サンドボックス実行レイヤ
   - `seatbelt`/`landlock` を用いた安全実行
   - 環境制御（ネットワーク許可/拒否、ワークスペース書き込み範囲など）

3. 承認フローハンドラ
   - “実行前承認” をHTTP/APIベースで挟み、承認結果に応じてブローカが実行を継続/中止

4. 履歴/要約/文脈レイヤ
   - 会話履歴の永続化・要約・プロジェクト文脈（`AGENTS.md`/ドキュメント）の注入
   - `conversation_manager.rs` や `project_doc.rs` に相当する機能のAPI化

5. apply_patch / ファイル操作の安全適用
   - 差分の提示・確認・適用、ロールバック

> これらを段階的にAPIエンドポイント群として提供することで、codex-cli に近いエージェント体験を再現可能です。本APIは「モデルとの会話/ストリーミング」を最小核とし、上位の運用層（ツール実行・承認・履歴・サンドボックス）を今後追加できるように分離しています。

### 参照コード（再掲）
- モデル呼び出し/ストリーミング核: `core/src/client.rs`, `core/src/client_common.rs`, `core/src/chat_completions.rs`
- 認証/アカウントIDヘッダ/URL/再試行: `core/src/model_provider_info.rs`, `login/*`
- コマンド実行・サンドボックス: `core/src/exec_command/*`, `core/src/exec_env.rs`, `core/src/shell.rs`, `core/src/seatbelt.rs`, `core/src/landlock.rs`
- MCP連携: `mcp-client`, `core/src/mcp_connection_manager.rs`, `core/src/mcp_tool_call.rs`
- 会話・履歴・文脈: `core/src/conversation_history.rs`, `core/src/conversation_manager.rs`, `core/src/project_doc.rs`
- 特殊ツール: `core/src/apply_patch.rs`, `core/src/openai_tools.rs`

---

## 結論
- 本APIサーバーは codex の「推論ストリーミング層」を再現しています（モデル通信・ツールイベントの流通・認証・リトライ）。
- 一方で、**コマンド実行・サンドボックス・承認フロー・MCP連携・履歴/要約・ファイル編集適用など、CLI/TUI が担う高次のエージェント機能は未実装**です。
- これらは今後、ツール実行ブローカや状態管理レイヤをAPIとして拡張することで段階的に再現可能です。
