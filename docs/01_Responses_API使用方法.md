# Codex-CLI におけるResponses API使用方法

このドキュメントでは、Codex-CLIがOpenAI Responses APIをどのように使用しているかについて、ソースコードに基づいた詳細な分析を提供します。

## 目次
- [概要](#概要)
- [API実装の全体像](#api実装の全体像)
- [認証機構](#認証機構)
- [リクエスト構造](#リクエスト構造)
- [ストリーミング処理](#ストリーミング処理)
- [エラーハンドリング](#エラーハンドリング)
- [ツール統合](#ツール統合)
- [実装ファイルマッピング](#実装ファイルマッピング)

---

## 概要

Codex-CLIは、OpenAI Responses API（実験的API）を主要なモデル通信手段として使用しています。このAPIは従来のChat Completions APIとは異なり、より高度なツール統合とストリーミング機能を提供します。

### 主な特徴
- **Server-Sent Events (SSE)**: リアルタイムストリーミングレスポンス
- **高度なツール統合**: ネイティブなファンクションコール対応
- **推論サマリー**: GPT-5系モデルでの推論過程の表示
- **プロンプトキャッシュ**: 効率的なコンテキスト管理

---

## API実装の全体像

### 核となる実装ファイル

**`codex-rs/core/src/client.rs`** (995行)
```rust
impl ModelClient {
    pub async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream> {
        match self.provider.wire_api {
            WireApi::Responses => self.stream_responses(prompt).await,
            WireApi::Chat => /* Chat Completions API実装 */,
        }
    }
}
```

### APIエンドポイント設定

Responses APIへのリクエストは以下の設定で行われます：

**HTTP ヘッダー**
```rust
req_builder = req_builder
    .header("OpenAI-Beta", "responses=experimental")
    .header("session_id", self.session_id.to_string())
    .header(reqwest::header::ACCEPT, "text/event-stream")
    .header("originator", originator)
    .header("User-Agent", get_codex_user_agent(Some(originator)));
```

---

## 認証機構

### 複数認証方式対応

Codex-CLIは以下の認証方式をサポートしています：

**1. OpenAI API Key認証**
```rust
let auth = auth_manager.as_ref().and_then(|m| m.auth());
let mut req_builder = self
    .provider
    .create_request_builder(&self.client, &auth)
    .await?;
```

**2. ChatGPT アカウント認証**
```rust
if let Some(auth) = auth.as_ref()
    && auth.mode == AuthMode::ChatGPT
    && let Some(account_id) = auth.get_account_id()
{
    req_builder = req_builder.header("chatgpt-account-id", account_id);
}
```

### 認証管理の特徴
- **自動トークンリフレッシュ**: 401エラー時の自動再認証
- **複数プロバイダー対応**: OpenAIとChatGPTの両方に対応
- **セキュアストレージ**: 認証情報の安全な保存

---

## リクエスト構造

### ResponsesApiRequest構造体

**`codex-rs/core/src/client_common.rs`** (136-156行)
```rust
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesApiRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) instructions: &'a str,
    pub(crate) input: &'a Vec<ResponseItem>,
    pub(crate) tools: &'a [serde_json::Value],
    pub(crate) tool_choice: &'static str,
    pub(crate) parallel_tool_calls: bool,
    pub(crate) reasoning: Option<Reasoning>,
    pub(crate) store: bool,
    pub(crate) stream: true,
    pub(crate) include: Vec<String>,
    pub(crate) prompt_cache_key: Option<String>,
    pub(crate) text: Option<TextControls>,
}
```

### 推論制御パラメータ

**GPT-5系モデル専用機能**
```rust
let reasoning = create_reasoning_param_for_request(
    &self.config.model_family,
    self.effort,
    self.summary,
);

// 推論内容の暗号化制御
let include: Vec<String> = if !store && reasoning.is_some() {
    vec!["reasoning.encrypted_content".to_string()]
} else {
    vec![]
};
```

---

## ストリーミング処理

### Server-Sent Events (SSE) パーサー

**イベント処理の核心実装** (`client.rs` 441-642行)
```rust
async fn process_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    idle_timeout: Duration,
) where S: Stream<Item = Result<Bytes>> + Unpin
```

### 主要イベントタイプ

**1. リアルタイムテキスト出力**
```rust
"response.output_text.delta" => {
    if let Some(delta) = event.delta {
        let event = ResponseEvent::OutputTextDelta(delta);
        if tx_event.send(Ok(event)).await.is_err() {
            return;
        }
    }
}
```

**2. 推論過程のストリーミング**
```rust
"response.reasoning_text.delta" => {
    if let Some(delta) = event.delta {
        let event = ResponseEvent::ReasoningContentDelta(delta);
        // 推論過程をリアルタイムで送信
    }
}
```

**3. ツール実行完了通知**
```rust
"response.output_item.done" => {
    let Ok(item) = serde_json::from_value::<ResponseItem>(item_val);
    let event = ResponseEvent::OutputItemDone(item);
    if tx_event.send(Ok(event)).await.is_err() {
        return;
    }
}
```

---

## エラーハンドリング

### 堅牢な再試行機構

**指数バックオフ戦略** (`client.rs` 222-361行)
```rust
let mut attempt = 0;
let max_retries = self.provider.request_max_retries();

loop {
    attempt += 1;
    match res {
        Ok(resp) if resp.status().is_success() => { /* 成功処理 */ },
        Ok(res) => {
            let status = res.status();
            
            // 利用制限エラーの特別処理
            if status == StatusCode::TOO_MANY_REQUESTS {
                if let Some(ErrorResponse { error }) = body {
                    if error.r#type.as_deref() == Some("usage_limit_reached") {
                        return Err(CodexErr::UsageLimitReached(/* ... */));
                    }
                }
            }
            
            // 再試行制限チェック
            if attempt > max_retries {
                return Err(CodexErr::RetryLimit(status));
            }
            
            let delay = backoff(attempt);
            tokio::time::sleep(delay).await;
        }
    }
}
```

### 専門的エラー処理

**1. 利用制限エラー**
- プラン種別の識別
- リセット時間の提供
- 適切なユーザー通知

**2. ストリームエラー**
- 接続切断の検出
- アイドルタイムアウト管理
- 部分的データの救済

---

## ツール統合

### ツール定義の動的生成

**`codex-rs/core/src/exec_command/responses_api.rs`** - カスタムツール定義
```rust
pub fn create_exec_command_tool_for_responses_api() -> ResponsesApiTool {
    let mut properties = BTreeMap::<String, JsonSchema>::new();
    properties.insert(
        "cmd".to_string(),
        JsonSchema::String {
            description: Some("The shell command to execute.".to_string()),
        },
    );
    
    ResponsesApiTool {
        name: EXEC_COMMAND_TOOL_NAME.to_owned(),
        description: r#"Execute shell commands on the local machine with streaming output."#
            .to_string(),
        strict: false,
        parameters: JsonSchema::Object { properties, required: /* ... */ },
    }
}
```

### ツール実行セッション管理

**ストリーミングコマンド実行**
```rust
pub fn create_write_stdin_tool_for_responses_api() -> ResponsesApiTool {
    // stdin書き込みツールの定義
    // セッションIDベースの管理
    // リアルタイム出力制御
}
```

---

## 実装ファイルマッピング

### コア実装ファイル

| ファイル | 責任範囲 | 主要機能 |
|----------|----------|----------|
| **`core/src/client.rs`** | API通信制御 | HTTPリクエスト、SSE処理、エラーハンドリング |
| **`core/src/client_common.rs`** | 共通データ構造 | リクエスト/レスポンス型、プロンプト管理 |
| **`core/src/openai_tools.rs`** | ツール定義 | JSONスキーマ生成、ツール統合 |
| **`exec_command/responses_api.rs`** | カスタムツール | exec_command、write_stdinツール |

### 支援モジュール

| モジュール | 機能 |
|-----------|------|
| **`user_agent.rs`** | HTTPユーザーエージェント生成 |
| **`model_provider_info.rs`** | プロバイダー設定管理 |
| **`error.rs`** | エラー型定義と変換 |

---

## パフォーマンス最適化

### プロンプトキャッシュ

**セッションベースキャッシュキー**
```rust
prompt_cache_key: Some(self.session_id.to_string())
```

### ストリーミング効率化

**チャネルベース非同期処理**
```rust
let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
tokio::spawn(process_sse(stream, tx_event, idle_timeout));
```

---

## まとめ

Codex-CLIのResponses API実装は、以下の点で高度な設計を示しています：

**技術的優位性**
- **非同期ストリーミング**: Server-Sent Eventsによるリアルタイム応答
- **堅牢なエラー処理**: 指数バックオフと専門的エラー分類
- **効率的認証管理**: 複数プロバイダー対応とトークン自動更新
- **動的ツール統合**: JSONスキーマベースのツール定義

**拡張性**
- プラガブルなプロバイダーアーキテクチャ
- モジュール化されたツールシステム
- 設定駆動型の機能制御

この実装により、Codex-CLIは高度なAIエージェント機能を提供しながら、安定性とパフォーマンスを両立させています。