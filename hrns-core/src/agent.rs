//! the rig agent loop, extracted from the CLI. streaming is reported through
//! the caller-supplied [`Observer`] (the CLI renders to stdout/stderr; hmux maps
//! to hub events) rather than printed directly, so the same loop drives both.

use crate::config::ToolsConfig;
use crate::permission::{PermissionResolver, PermissionsConfig};
use crate::tools::{builtin_tools, ToolMiddleware};
use futures_util::stream::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::message::{
    AssistantContent, DocumentSourceKind, Image, ImageDetail, ImageMediaType,
    Message as ChatMessage, ToolResultContent, UserContent,
};
use rig::providers;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use rig::tool::ToolDyn;
use rig::OneOrMany;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// how the loop reports what the model streams. the CLI renders to
/// stdout/stderr (text to stdout, reasoning/tools to stderr); an embedding host
/// (hmux) maps each call to a normalized hub event. one `run` uses one observer.
pub trait Observer: Send {
    fn text(&mut self, delta: &str);
    fn reasoning(&mut self, delta: &str);
    fn tool_call(&mut self, name: &str, args: &str);
    fn tool_result(&mut self, result: &str);
    fn error(&mut self, message: &str);
    /// a steer restarted the turn: close the current message so the continuation
    /// opens a fresh one (a no-op for a renderer that streams continuously).
    fn restart(&mut self);
    /// the stream ended (naturally or on error); flush any pending state.
    fn end_stream(&mut self);
}

/// (role, text) for each message in a rig history, so a consumer can re-seed a
/// forked/reverted session as normalized events. non-text content is dropped and
/// empty messages are skipped.
pub fn history_texts(history: &[ChatMessage]) -> Vec<(String, String)> {
    history
        .iter()
        .filter_map(|m| match m {
            ChatMessage::User { content } => {
                let text: String = content.iter().filter_map(user_text).collect();
                (!text.is_empty()).then(|| ("user".to_owned(), text))
            }
            ChatMessage::Assistant { content, .. } => {
                let text: String = content.iter().filter_map(assistant_text).collect();
                (!text.is_empty()).then(|| ("assistant".to_owned(), text))
            }
            // the system prompt is applied separately, not part of the re-seeded history.
            ChatMessage::System { .. } => None,
        })
        .collect()
}

fn user_text(c: &UserContent) -> Option<&str> {
    match c {
        UserContent::Text(t) => Some(t.text.as_str()),
        _ => None,
    }
}

fn assistant_text(c: &AssistantContent) -> Option<&str> {
    match c {
        AssistantContent::Text(t) => Some(t.text.as_str()),
        _ => None,
    }
}

/// an image attachment for a prompt: base64 data + its mime type, built from a
/// hmux FileRef's data: url by the backend. images only (R18).
pub struct Attachment {
    pub mime: String,
    pub base64: String,
}

/// map a mime type to a rig image media type (the kinds providers accept).
fn image_media_type(mime: &str) -> Option<ImageMediaType> {
    match mime {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        _ => None,
    }
}

/// everything a single turn needs. borrows its inputs; `run` owns nothing.
pub struct RunConfig<'a> {
    pub client_type: &'a str,
    pub model_name: &'a str,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub max_tokens: u64,
    pub max_turns: usize,
    pub tools: &'a ToolsConfig,
    pub tools_override: &'a Option<Vec<String>>,
    pub permissions: &'a PermissionsConfig,
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
    pub attachments: &'a [Attachment],
    /// extra provider request params merged into the body (e.g. openai
    /// `{"reasoning_effort": "high"}`); the caller maps a reasoning level to it.
    pub additional_params: Option<serde_json::Value>,
}

/// what a turn produced: the loop exit cause (for before-stop handling) and the
/// updated conversation history (the prior history plus this turn's messages), so
/// the caller can drive the next turn WITH context. the CLI ignores the history
/// (one-shot); the hmux backend stores it per session for follow-up turns.
pub struct RunOutcome {
    pub exit_reason: &'static str,
    pub exit_error: Option<String>,
    pub history: Vec<ChatMessage>,
}

/// streams one turn: builds the provider client + agent, adds the tool set, and
/// reports the stream through `observer`. returns the loop exit cause
/// (`"stop"`/`"error"`, and the error message) for the caller's before-stop
/// handling. built-in read/bash come from `builtin_tools`; hook-registered
/// tools come from `middleware.extra_tools()`.
pub async fn run(
    cfg: RunConfig<'_>,
    resolver: Arc<dyn PermissionResolver>,
    middleware: Arc<dyn ToolMiddleware>,
    observer: &mut dyn Observer,
    mut history: Vec<ChatMessage>,
    cancel: CancellationToken,
    mut steer_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    // rig's typestate builder requires .api_key() before .build(); when no key
    // is configured, pass a placeholder so local openai-compatible servers
    // (which ignore the auth header) still work. hosted providers return a clear
    // auth error from upstream at request time.
    let api_key = if cfg.api_key.is_empty() {
        "none"
    } else {
        cfg.api_key
    };

    // the whole tool set is dynamic (hook tools + gated built-ins), so it does
    // not change the agent builder's type and one `.tools()` call suffices.
    let mut tools: Vec<Box<dyn ToolDyn>> = middleware.extra_tools();
    tools.extend(builtin_tools(
        cfg.tools,
        cfg.tools_override,
        cfg.permissions,
        &resolver,
        &middleware,
    ));

    // build the user turn: the prompt text plus any image attachments (R18).
    let mut parts: Vec<UserContent> = vec![UserContent::text(cfg.user_prompt)];
    for att in cfg.attachments {
        parts.push(UserContent::Image(Image {
            data: DocumentSourceKind::Base64(att.base64.clone()),
            media_type: image_media_type(&att.mime),
            // openai's chat-completions image path requires a detail; default to auto.
            detail: Some(ImageDetail::Auto),
            additional_params: None,
        }));
    }
    let prompt = ChatMessage::User {
        content: OneOrMany::many(parts).expect("prompt has at least the text part"),
    };

    macro_rules! build_openai_client {
        () => {{
            let mut builder = providers::openai::Client::builder().api_key(api_key);
            if let Some(url) = cfg.base_url {
                builder = builder.base_url(url);
            }
            builder.build().expect("failed to build OpenAI client")
        }};
    }

    // drives the streamed turn(s), reporting each item through the observer. an outer
    // loop restarts the stream on a steer (a redirect mid-generation).
    macro_rules! stream_loop {
        ($agent:expr) => {{
            let mut exit_reason: &'static str = "stop";
            let mut exit_error: Option<String> = None;
            let mut next_prompt: Option<ChatMessage> = Some(prompt);
            'turn: while let Some(cur_prompt) = next_prompt.take() {
                let cur_clone = cur_prompt.clone();
                let mut stream = $agent.stream_chat(cur_prompt, &history).await;
                let mut partial = String::new();
                loop {
                    let chunk_result = tokio::select! {
                        biased;
                        // abort: stop iterating; end_stream closes the message and run
                        // returns with exit_reason "aborted".
                        _ = cancel.cancelled() => {
                            exit_reason = "aborted";
                            break 'turn;
                        }
                        // steer: preserve the interrupted turn (its prompt + partial reply)
                        // in history, then restart with the steer text as a new user turn --
                        // all within one run() call (same processing span, no idle).
                        Some(text) = steer_rx.recv() => {
                            history.push(cur_clone.clone());
                            if !partial.is_empty() {
                                history.push(ChatMessage::assistant(std::mem::take(&mut partial)));
                            }
                            observer.restart();
                            next_prompt = Some(ChatMessage::user(text));
                            continue 'turn;
                        }
                        item = stream.next() => match item {
                            Some(c) => c,
                            None => break,
                        },
                    };
                    match chunk_result {
                        Ok(MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::Text(t),
                        )) => {
                            partial.push_str(&t.text);
                            observer.text(&t.text);
                        }
                        Ok(MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                        )) => {
                            observer.reasoning(&reasoning);
                        }
                        Ok(MultiTurnStreamItem::StreamAssistantItem(
                            StreamedAssistantContent::ToolCall { tool_call, .. },
                        )) => {
                            observer.tool_call(
                                &tool_call.function.name,
                                &format!("{}", tool_call.function.arguments),
                            );
                        }
                        Ok(MultiTurnStreamItem::StreamUserItem(
                            StreamedUserContent::ToolResult { tool_result, .. },
                        )) => {
                            let result_text: String = tool_result
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            observer.tool_result(&result_text);
                        }
                        Ok(MultiTurnStreamItem::FinalResponse(fin)) => {
                            // append this turn's messages so the next turn runs with context.
                            history.extend(fin.history().unwrap_or_default().iter().cloned());
                        }
                        Ok(_) => {}
                        Err(e) => {
                            observer.error(&format!("{}", e));
                            exit_reason = "error";
                            exit_error = Some(format!("{}", e));
                            break 'turn;
                        }
                    }
                }
            }
            observer.end_stream();
            (exit_reason, exit_error)
        }};
    }

    // builds the agent on a provider builder, adds the tool set, and drives it.
    macro_rules! build_and_drive {
        ($builder:expr) => {{
            let mut builder = $builder
                .max_tokens(cfg.max_tokens)
                .default_max_turns(cfg.max_turns);
            if let Some(params) = &cfg.additional_params {
                builder = builder.additional_params(params.clone());
            }
            if !cfg.system_prompt.is_empty() {
                builder = builder.preamble(cfg.system_prompt);
            }
            let agent = builder.tools(tools).build();
            stream_loop!(agent)
        }};
    }

    let exit = match cfg.client_type {
        "openai_completions" => {
            let client = build_openai_client!();
            let model = client.completion_model(cfg.model_name).completions_api();
            build_and_drive!(rig::agent::AgentBuilder::new(model))
        }
        "openai" | "openai_responses" => {
            let client = build_openai_client!();
            build_and_drive!(client.agent(cfg.model_name))
        }
        "anthropic" => {
            let mut builder = providers::anthropic::Client::builder().api_key(api_key);
            if let Some(url) = cfg.base_url {
                builder = builder.base_url(url);
            }
            let client = builder.build().expect("failed to build Anthropic client");
            build_and_drive!(client.agent(cfg.model_name))
        }
        "gemini" => {
            let mut builder = providers::gemini::Client::builder().api_key(api_key);
            if let Some(url) = cfg.base_url {
                builder = builder.base_url(url);
            }
            let client = builder.build().expect("failed to build Gemini client");
            build_and_drive!(client.agent(cfg.model_name))
        }
        "cohere" => {
            let mut builder = providers::cohere::Client::builder().api_key(api_key);
            if let Some(url) = cfg.base_url {
                builder = builder.base_url(url);
            }
            let client = builder.build().expect("failed to build Cohere client");
            build_and_drive!(client.agent(cfg.model_name))
        }
        "xai" => {
            let mut builder = providers::xai::Client::builder().api_key(api_key);
            if let Some(url) = cfg.base_url {
                builder = builder.base_url(url);
            }
            let client = builder.build().expect("failed to build xAI client");
            build_and_drive!(client.agent(cfg.model_name))
        }
        other => return Err(format!("unsupported client type: {}", other).into()),
    };
    Ok(RunOutcome {
        exit_reason: exit.0,
        exit_error: exit.1,
        history,
    })
}
