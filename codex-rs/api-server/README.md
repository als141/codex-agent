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

## 実装の詳細

### 最小変更の方針
- 既存の `codex-core` クレートは変更せず、公開APIのみを利用
- 新規追加は `api-server` クレートのみ
- 認証・モデル通信・設定管理は既存実装を再利用

### 型の可視性問題への対処
- `ResponseStream` が crate-private のため、`ModelClient::stream_owned` を追加
- APIサーバー側では汎用的な `Stream<Item = Result<ResponseEvent>>` として扱う
- `Arc<ModelClient>` を使用してライフタイム問題を回避

### CORS設定
全オリジン・全メソッド・全ヘッダーを許可（開発用途）。本番環境では適切に制限してください。

## テスト例

```bash
# ヘルスチェック
curl http://localhost:8080/healthz

# 非ストリーミングチャット
curl -X POST http://localhost:8080/chat \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"こんにちは"}]}'

# ストリーミングチャット
curl -X POST http://localhost:8080/chat/stream \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"messages":[{"role":"user","content":"今日の天気は？"}]}'
```

## 今後の拡張案
- 認証ミドルウェア（Bearer トークン等）
- レート制限
- メトリクス・監視
- WebSocket サポート
- マルチテナント対応（複数の `CODEX_HOME`）
- プロキシ設定のサポート
