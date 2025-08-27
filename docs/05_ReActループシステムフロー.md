# Codex-CLI ReActループシステムフローの詳細分析

このドキュメントでは、Codex-CLIにおけるReAct（Reasoning and Acting）ループの実装について、セッション管理、タスク実行、ツール呼び出し、エラーハンドリングの観点から詳細に分析します。

## 目次
- [概要](#概要)
- [ReActアーキテクチャ](#reactアーキテクチャ)
- [セッション管理](#セッション管理)  
- [タスク実行サイクル](#タスク実行サイクル)
- [ターン処理](#ターン処理)
- [ツール実行フロー](#ツール実行フロー)
- [イベント駆動システム](#イベント駆動システム)
- [エラーハンドリング](#エラーハンドリング)
- [実装ファイルマッピング](#実装ファイルマッピング)

---

## 概要

Codex-CLIは、ReAct（Reasoning and Acting）パラダムに基づく高度なエージェントループを実装しています。このシステムは、AI モデルの推論能力とツール実行能力を循環的に組み合わせることで、複雑なタスクの自律的解決を可能にします。

### ReActパラダム
1. **Reasoning**: モデルが現在の状況を分析し、次のアクションを計画
2. **Acting**: 計画されたアクションを実際のツールを通じて実行
3. **Observing**: ツール実行結果を観察・評価
4. **Iterating**: 結果に基づいて次の推論・行動サイクルへ移行

---

## ReActアーキテクチャ

### 核となる構成要素

**`codex-rs/core/src/codex.rs`** - Session構造体 (280-300行)
```rust
/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    session_id: Uuid,
    tx_event: Sender<Event>,
    /// Manager for external MCP servers/tools.
    mcp_connection_manager: McpConnectionManager,
    session_manager: ExecSessionManager,
    /// Optional rollout recorder for persisting the conversation transcript
    rollout: Mutex<Option<RolloutRecorder>>,
    state: Mutex<State>,
    codex_linux_sandbox_exe: Option<PathBuf>,
    user_shell: shell::Shell,
    show_raw_agent_reasoning: bool,
}
```

### システムレベル状態管理

**State構造体による状態追跡**
```rust
struct State {
    client: ModelClient,
    conversation_history: ConversationHistory,
    current_task: Option<TaskInfo>,
    approval_requests: Vec<PendingApproval>,
    tool_execution_state: ToolExecutionState,
}
```

---

## セッション管理

### セッション初期化プロセス

**`Session::new`メソッド** (codex.rs)
```rust
async fn new(
    configure_session: ConfigureSession,
    config: Arc<Config>,
    auth_manager: Arc<AuthManager>,
    tx_event: Sender<Event>,
    initial_history: Option<Vec<ResponseItem>>,
) -> anyhow::Result<(Arc<Self>, TurnContext)> {
    // 1. モデルクライアント初期化
    let client = ModelClient::from_config(/* ... */).await?;
    
    // 2. MCP接続マネージャ初期化
    let mcp_connection_manager = McpConnectionManager::new(/* ... */);
    
    // 3. ツール設定構築
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_family: &client.get_model_family(),
        approval_policy,
        sandbox_policy,
        /* ... */
    });
    
    // 4. 初期履歴設定
    let mut conversation_history = ConversationHistory::new();
    if let Some(initial_history) = initial_history {
        conversation_history.record_items(&initial_history);
    }
    
    Ok((session, turn_context))
}
```

### TurnContextによる実行環境

**ターン固有の設定管理**
```rust
struct TurnContext {
    client: ModelClient,
    tools_config: ToolsConfig,
    approval_policy: AskForApproval,
    sandbox_policy: SandboxPolicy,
    cwd: PathBuf,
    base_instructions: Option<String>,
    disable_response_storage: bool,
}
```

---

## タスク実行サイクル

### メインタスクループ

**`run_task`関数** - ReActループの最上位レベル
```rust
async fn run_task(
    sess: Arc<Session>,
    turn_context: &TurnContext,
    sub_id: String,
    input: Vec<InputItem>,
) {
    if input.is_empty() {
        return;
    }

    // 1. タスク開始イベント送信
    let event = Event {
        id: sub_id.clone(),
        msg: EventMsg::TaskStarted(TaskStartedEvent {
            model_context_window: turn_context.client.get_model_context_window(),
        }),
    };
    sess.tx_event.send(event).await.ok();

    // 2. 初期入力のResponseItemへの変換
    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
    sess.record_conversation_items(&[initial_input_for_turn.clone().into()]);

    // 3. ターン履歴構築
    let turn_input: Vec<ResponseItem> =
        sess.turn_input_with_history(vec![initial_input_for_turn.into()]);

    let mut turn_diff_tracker = TurnDiffTracker::new(turn_context.cwd.clone());
    let mut input = turn_input;

    // 4. ReActループ実行
    loop {
        match run_turn(
            &sess,
            turn_context,
            &mut turn_diff_tracker,
            sub_id.clone(),
            input,
        ).await {
            Ok(turn_output) => {
                // ターン完了処理
                let (new_input, should_continue) = 
                    process_turn_output(sess.clone(), turn_output).await;
                    
                if !should_continue {
                    break;
                }
                input = new_input;
            }
            Err(e) => {
                // エラーハンドリング
                handle_turn_error(sess.clone(), &sub_id, e).await;
                break;
            }
        }
    }

    // 5. タスク完了イベント送信
    let event = Event {
        id: sub_id,
        msg: EventMsg::TaskComplete(TaskCompleteEvent { /* ... */ }),
    };
    sess.tx_event.send(event).await.ok();
}
```

---

## ターン処理

### 単一ターン実行

**`run_turn`関数** - 1回の推論・行動サイクル
```rust
async fn run_turn(
    sess: &Session,
    turn_context: &TurnContext,
    turn_diff_tracker: &mut TurnDiffTracker,
    sub_id: String,
    input: Vec<ResponseItem>,
) -> CodexResult<Vec<ProcessedResponseItem>> {
    // 1. 利用可能ツールの取得
    let tools = get_openai_tools(
        &turn_context.tools_config,
        Some(sess.mcp_connection_manager.list_all_tools()),
    );

    // 2. プロンプト構築
    let prompt = Prompt {
        input,
        store: !turn_context.disable_response_storage,
        tools,
        base_instructions_override: turn_context.base_instructions.clone(),
    };

    // 3. モデル推論の実行（リトライ機能付き）
    let mut retries = 0;
    loop {
        match turn_context.client.stream(&prompt).await {
            Ok(response_stream) => {
                // 4. レスポンスストリーム処理
                return process_response_stream(
                    sess,
                    turn_context,
                    turn_diff_tracker,
                    sub_id,
                    response_stream,
                ).await;
            }
            Err(e) if should_retry_error(&e) && retries < MAX_RETRIES => {
                retries += 1;
                let delay = backoff_delay(retries);
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
```

### レスポンス処理パイプライン

**ストリーミングレスポンスの段階的処理**
```rust
async fn process_response_stream(
    sess: &Session,
    turn_context: &TurnContext,
    turn_diff_tracker: &mut TurnDiffTracker,
    sub_id: String,
    mut stream: ResponseStream,
) -> CodexResult<Vec<ProcessedResponseItem>> {
    let mut processed_items = Vec::new();
    let mut current_reasoning = String::new();
    let mut current_message = String::new();

    while let Some(event) = stream.next().await {
        match event? {
            // テキスト出力の段階的構築
            ResponseEvent::OutputTextDelta(delta) => {
                current_message.push_str(&delta);
                sess.emit_agent_message_delta(sub_id.clone(), delta).await;
            }
            
            // 推論過程の可視化（GPT-5系）
            ResponseEvent::ReasoningContentDelta(delta) => {
                current_reasoning.push_str(&delta);
                sess.emit_reasoning_delta(sub_id.clone(), delta).await;
            }
            
            // 完成アイテムの処理
            ResponseEvent::OutputItemDone(item) => {
                let processed_item = process_response_item(
                    sess,
                    turn_context,
                    turn_diff_tracker,
                    sub_id.clone(),
                    item,
                ).await;
                processed_items.push(processed_item);
            }
            
            ResponseEvent::Completed { .. } => break,
        }
    }

    Ok(processed_items)
}
```

---

## ツール実行フロー

### ファンクションコールディスパッチ

**`handle_function_call`関数** - ツール呼び出しの中央ハンドラー
```rust
async fn handle_function_call(
    sess: &Session,
    turn_context: &TurnContext,
    turn_diff_tracker: &mut TurnDiffTracker,
    sub_id: String,
    name: String,
    arguments: String,
    call_id: String,
) -> ResponseInputItem {
    match name.as_str() {
        // シェルコマンド実行
        "container.exec" | "shell" => {
            let params = parse_container_exec_arguments(arguments, turn_context, &call_id)?;
            handle_container_exec_with_params(
                params, sess, turn_context, turn_diff_tracker, sub_id, call_id
            ).await
        }
        
        // パッチ適用
        "apply_patch" => {
            let args: ApplyPatchToolArgs = serde_json::from_str(&arguments)?;
            let exec_params = ExecParams {
                command: vec!["apply_patch".to_string(), args.input],
                cwd: turn_context.cwd.clone(),
                /* ... */
            };
            handle_container_exec_with_params(/* ... */).await
        }
        
        // プラン更新
        "update_plan" => {
            handle_update_plan(sess, arguments, sub_id, call_id).await
        }
        
        // Webサーチ
        "web_search" => {
            handle_web_search(sess, arguments, sub_id, call_id).await
        }
        
        // MCP外部ツール
        tool_name if tool_name.contains('/') => {
            handle_mcp_tool_call(
                &sess.mcp_connection_manager,
                tool_name.to_string(),
                arguments,
                call_id,
            ).await
        }
        
        // 未知のツール
        unknown => {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: FunctionCallOutputPayload {
                    content: format!("Unknown function: {unknown}"),
                    success: Some(false),
                },
            }
        }
    }
}
```

### シェルコマンド実行詳細

**`handle_container_exec_with_params`** - 実際のコマンド実行
```rust
async fn handle_container_exec_with_params(
    params: ExecParams,
    sess: &Session,
    turn_context: &TurnContext,
    turn_diff_tracker: &mut TurnDiffTracker,
    sub_id: String,
    call_id: String,
) -> ResponseInputItem {
    // 1. セキュリティ評価
    let safety_check = assess_command_safety(&params, turn_context).await;
    
    // 2. 承認プロセス（必要に応じて）
    if requires_approval(&safety_check, &turn_context.approval_policy) {
        let approval = request_user_approval(sess, &params, &sub_id).await;
        if !approval.approved {
            return create_rejection_response(call_id, approval.reason);
        }
    }

    // 3. サンドボックス設定
    let sandbox_type = determine_sandbox_type(&params, &turn_context.sandbox_policy);

    // 4. 実行コンテキスト構築
    let exec_args = ExecInvokeArgs {
        params: &params,
        sandbox_type,
        sandbox_policy: &turn_context.sandbox_policy,
        codex_linux_sandbox_exe: sess.codex_linux_sandbox_exe.as_ref(),
        stdout_stream: Some(/* ストリーミング出力 */),
    };

    // 5. 実際の実行
    let begin_ctx = ExecCommandContext {
        sub_id: sub_id.clone(),
        call_id: call_id.clone(),
        command: params.command.clone(),
        apply_patch: detect_apply_patch_operation(&params),
    };

    sess.run_exec_with_events(turn_diff_tracker, begin_ctx, exec_args).await
}
```

---

## イベント駆動システム

### イベント型定義

**`codex-rs/protocol/src/protocol.rs`** - イベントメッセージ体系
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    // セッション関連
    SessionConfigured(SessionConfiguredEvent),
    
    // タスクライフサイクル
    TaskStarted(TaskStartedEvent),
    TaskComplete(TaskCompleteEvent),
    TurnAborted(TurnAbortedEvent),
    
    // エージェントメッセージ
    AgentMessage(AgentMessageEvent),
    AgentMessageDelta(AgentMessageDeltaEvent),
    
    // 推論過程（GPT-5系）
    AgentReasoning(AgentReasoningEvent),
    AgentReasoningDelta(AgentReasoningDeltaEvent),
    
    // ツール実行
    ExecCommandBegin(ExecCommandBeginEvent),
    ExecCommandEnd(ExecCommandEndEvent),
    
    // 承認要求
    ExecApprovalRequest(ExecApprovalRequestEvent),
    PatchApprovalRequest(PatchApprovalRequestEvent),
    
    // プランニング
    PlanUpdate(PlanUpdateEvent),
    
    // エラー
    Error(ErrorEvent),
    StreamError(StreamErrorEvent),
}
```

### リアルタイムイベント配信

**イベントチャネルによる非ブロッキング通信**
```rust
impl Session {
    pub(crate) async fn emit_agent_message_delta(
        &self,
        sub_id: String,
        delta: String,
    ) {
        let event = Event {
            id: sub_id,
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta,
            }),
        };
        self.tx_event.send(event).await.ok();
    }

    pub(crate) async fn emit_exec_begin(
        &self,
        sub_id: String,
        command: Vec<String>,
    ) {
        let event = Event {
            id: sub_id,
            msg: EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                command,
                timestamp: chrono::Utc::now(),
            }),
        };
        self.tx_event.send(event).await.ok();
    }
}
```

---

## エラーハンドリング

### 階層的エラー処理

**複数レベルでの回復戦略**
```rust
// 1. ツールレベルエラー
async fn handle_tool_error(
    error: ToolExecutionError,
    call_id: String,
) -> ResponseInputItem {
    match error {
        ToolExecutionError::Timeout => {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: FunctionCallOutputPayload {
                    content: "Command timed out".to_string(),
                    success: Some(false),
                },
            }
        }
        ToolExecutionError::PermissionDenied => {
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: FunctionCallOutputPayload {
                    content: "Permission denied - consider using escalated permissions".to_string(),
                    success: Some(false),
                },
            }
        }
        ToolExecutionError::SandboxViolation(details) => {
            // サンドボックス違反の詳細をモデルに提供
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output: FunctionCallOutputPayload {
                    content: format!("Sandbox violation: {details}"),
                    success: Some(false),
                },
            }
        }
    }
}

// 2. ターンレベルエラー
async fn handle_turn_error(
    sess: Arc<Session>,
    sub_id: &str,
    error: CodexErr,
) {
    match error {
        CodexErr::RetryLimit(_) => {
            sess.emit_error_event(sub_id.to_string(), 
                "Maximum retries exceeded".to_string()).await;
        }
        CodexErr::UsageLimitReached(details) => {
            sess.emit_usage_limit_event(sub_id.to_string(), details).await;
        }
        _ => {
            sess.emit_error_event(sub_id.to_string(), 
                format!("Turn execution failed: {error}")).await;
        }
    }
}
```

### 回復メカニズム

**自動リトライとフォールバック**
```rust
async fn execute_with_recovery<F, T>(
    mut operation: F,
    max_retries: usize,
) -> Result<T, CodexErr>
where
    F: FnMut() -> Pin<Box<dyn Future<Output = Result<T, CodexErr>>>>,
{
    let mut retries = 0;
    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if retries < max_retries && should_retry(&e) => {
                retries += 1;
                let delay = exponential_backoff(retries);
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
```

---

## 実装ファイルマッピング

### 核となる実装ファイル

| ファイル | 責任範囲 | 主要機能 |
|----------|----------|----------|
| **`core/src/codex.rs`** | ReActループ制御 | Session管理、ターン実行、ツールディスパッチ |
| **`protocol/src/protocol.rs`** | イベントシステム | メッセージ型定義、イベント配信 |
| **`core/src/client.rs`** | モデル通信 | API呼び出し、ストリーミング処理 |
| **`core/src/exec.rs`** | ツール実行 | コマンド実行、サンドボックス管理 |

### 支援モジュール

| モジュール | 機能 |
|-----------|------|
| **`conversation_history.rs`** | 履歴管理 |
| **`safety.rs`** | セキュリティ評価 |
| **`turn_diff_tracker.rs`** | 差分追跡 |
| **`mcp_connection_manager.rs`** | 外部ツール管理 |

---

## パフォーマンス最適化

### 並列処理

**非ブロッキング実行**
```rust
// ツール実行と出力ストリーミングの並列化
let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel(1024);
let exec_future = tokio::spawn(async move {
    process_exec_tool_call(params, sandbox_type, stdout_tx).await
});
let stream_future = tokio::spawn(async move {
    stream_command_output(stdout_rx, event_sender).await
});

// 両方の完了を待機
let (exec_result, _) = tokio::try_join!(exec_future, stream_future)?;
```

### メモリ効率化

**適応的履歴管理**
```rust
// コンテキストウィンドウに基づく履歴圧縮
let context_window = turn_context.client.get_model_context_window();
let max_history_tokens = context_window * 0.6; // 60%をアイテム履歴に利用

sess.conversation_history.optimize_for_context_window(max_history_tokens);
```

---

## まとめ

Codex-CLIのReActループシステムは、以下の特徴により高度な自律性と信頼性を実現しています：

**アーキテクチャの優位性**
- **イベント駆動**: 非ブロッキングな反応型システム
- **階層的エラー処理**: 多層的な回復メカニズム
- **型安全性**: Rustによる実行時安全性保証
- **拡張性**: プラガブルなツールとMCP統合

**実行効率性**
- **並列処理**: ツール実行と出力処理の同期化
- **適応的管理**: コンテキストウィンドウに基づく最適化
- **リソース制御**: タイムアウトとサンドボックス機能

**ユーザビリティ**
- **リアルタイムフィードバック**: 段階的な進捗表示
- **透明性**: 推論過程と実行ステップの可視化
- **制御性**: ユーザー承認とインタラプト機能

この実装により、Codex-CLIは複雑な開発タスクを安全かつ効率的に処理する高品質なAIエージェント体験を提供しています。