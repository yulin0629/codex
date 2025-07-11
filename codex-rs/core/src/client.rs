use std::io::BufRead;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures::prelude::*;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::io::ReaderStream;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::chat_completions::AggregateStreamExt;
use crate::chat_completions::stream_chat_completions;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::client_common::ResponsesApiRequest;
use crate::client_common::create_reasoning_param_for_request;
use crate::config::Config;
use crate::config_types::ReasoningEffort as ReasoningEffortConfig;
use crate::config_types::ReasoningSummary as ReasoningSummaryConfig;
use crate::error::CodexErr;
use crate::error::Result;
use crate::flags::CODEX_RS_SSE_FIXTURE;
use crate::flags::OPENAI_REQUEST_MAX_RETRIES;
use crate::flags::OPENAI_STREAM_IDLE_TIMEOUT_MS;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::models::ResponseItem;
use crate::openai_tools::create_tools_json_for_responses_api;
use crate::protocol::TokenUsage;
use crate::util::backoff;
use std::sync::Arc;

#[derive(Clone)]
pub struct ModelClient {
    config: Arc<Config>,
    client: reqwest::Client,
    provider: ModelProviderInfo,
    effort: ReasoningEffortConfig,
    summary: ReasoningSummaryConfig,
}

impl ModelClient {
    pub fn new(
        config: Arc<Config>,
        provider: ModelProviderInfo,
        effort: ReasoningEffortConfig,
        summary: ReasoningSummaryConfig,
    ) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            provider,
            effort,
            summary,
        }
    }

    /// Dispatches to either the Responses or Chat implementation depending on
    /// the provider config.  Public callers always invoke `stream()` – the
    /// specialised helpers are private to avoid accidental misuse.
    pub async fn stream(&self, prompt: &Prompt) -> Result<ResponseStream> {
        match self.provider.wire_api {
            WireApi::Responses => self.stream_responses(prompt).await,
            WireApi::Chat => {
                // Create the raw streaming connection first.
                let response_stream = stream_chat_completions(
                    prompt,
                    &self.config.model,
                    &self.client,
                    &self.provider,
                )
                .await?;

                // Wrap it with the aggregation adapter so callers see *only*
                // the final assistant message per turn (matching the
                // behaviour of the Responses API).
                let mut aggregated = response_stream.aggregate();

                // Bridge the aggregated stream back into a standard
                // `ResponseStream` by forwarding events through a channel.
                let (tx, rx) = mpsc::channel::<Result<ResponseEvent>>(16);

                tokio::spawn(async move {
                    use futures::StreamExt;
                    while let Some(ev) = aggregated.next().await {
                        // Exit early if receiver hung up.
                        if tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(ResponseStream { rx_event: rx })
            }
        }
    }

    /// Implementation for the OpenAI *Responses* experimental API.
    async fn stream_responses(&self, prompt: &Prompt) -> Result<ResponseStream> {
        if let Some(path) = &*CODEX_RS_SSE_FIXTURE {
            // short circuit for tests
            warn!(path, "Streaming from fixture");
            return stream_from_fixture(path).await;
        }

        let full_instructions = prompt.get_full_instructions(&self.config.model);
        let tools_json = create_tools_json_for_responses_api(prompt, &self.config.model)?;
        let reasoning = create_reasoning_param_for_request(&self.config, self.effort, self.summary);
        let payload = ResponsesApiRequest {
            model: &self.config.model,
            instructions: &full_instructions,
            input: &prompt.input,
            tools: &tools_json,
            tool_choice: "auto",
            parallel_tool_calls: false,
            reasoning,
            previous_response_id: prompt.prev_id.clone(),
            store: prompt.store,
            stream: true,
        };

        trace!(
            "POST to {}: {}",
            self.provider.get_full_url(),
            serde_json::to_string(&payload)?
        );

        let mut attempt = 0;
        loop {
            attempt += 1;

            let req_builder = self
                .provider
                .create_request_builder(&self.client)?
                .header("OpenAI-Beta", "responses=experimental")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .json(&payload);

            let res = req_builder.send().await;
            match res {
                Ok(resp) if resp.status().is_success() => {
                    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);

                    // spawn task to process SSE
                    let stream = resp.bytes_stream().map_err(CodexErr::Reqwest);
                    tokio::spawn(process_sse(stream, tx_event));

                    return Ok(ResponseStream { rx_event });
                }
                Ok(res) => {
                    let status = res.status();
                    // The OpenAI Responses endpoint returns structured JSON bodies even for 4xx/5xx
                    // errors. When we bubble early with only the HTTP status the caller sees an opaque
                    // "unexpected status 400 Bad Request" which makes debugging nearly impossible.
                    // Instead, read (and include) the response text so higher layers and users see the
                    // exact error message (e.g. "Unknown parameter: 'input[0].metadata'"). The body is
                    // small and this branch only runs on error paths so the extra allocation is
                    // negligible.
                    if !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) {
                        // Surface the error body to callers. Use `unwrap_or_default` per Clippy.
                        let body = res.text().await.unwrap_or_default();
                        return Err(CodexErr::UnexpectedStatus(status, body));
                    }

                    if attempt > *OPENAI_REQUEST_MAX_RETRIES {
                        return Err(CodexErr::RetryLimit(status));
                    }

                    // Pull out Retry‑After header if present.
                    let retry_after_secs = res
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    let delay = retry_after_secs
                        .map(|s| Duration::from_millis(s * 1_000))
                        .unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    if attempt > *OPENAI_REQUEST_MAX_RETRIES {
                        return Err(e.into());
                    }
                    let delay = backoff(attempt);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    response: Option<Value>,
    item: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ResponseCreated {}

#[derive(Debug, Deserialize)]
struct ResponseCompleted {
    id: String,
    usage: Option<ResponseCompletedUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedUsage {
    input_tokens: u64,
    input_tokens_details: Option<ResponseCompletedInputTokensDetails>,
    output_tokens: u64,
    output_tokens_details: Option<ResponseCompletedOutputTokensDetails>,
    total_tokens: u64,
}

impl From<ResponseCompletedUsage> for TokenUsage {
    fn from(val: ResponseCompletedUsage) -> Self {
        TokenUsage {
            input_tokens: val.input_tokens,
            cached_input_tokens: val.input_tokens_details.map(|d| d.cached_tokens),
            output_tokens: val.output_tokens,
            reasoning_output_tokens: val.output_tokens_details.map(|d| d.reasoning_tokens),
            total_tokens: val.total_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedInputTokensDetails {
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedOutputTokensDetails {
    reasoning_tokens: u64,
}

async fn process_sse<S>(stream: S, tx_event: mpsc::Sender<Result<ResponseEvent>>)
where
    S: Stream<Item = Result<Bytes>> + Unpin,
{
    let mut stream = stream.eventsource();

    // If the stream stays completely silent for an extended period treat it as disconnected.
    let idle_timeout = *OPENAI_STREAM_IDLE_TIMEOUT_MS;
    // The response id returned from the "complete" message.
    let mut response_completed: Option<ResponseCompleted> = None;

    loop {
        let sse = match timeout(idle_timeout, stream.next()).await {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let event = CodexErr::Stream(e.to_string());
                let _ = tx_event.send(Err(event)).await;
                return;
            }
            Ok(None) => {
                match response_completed {
                    Some(ResponseCompleted {
                        id: response_id,
                        usage,
                    }) => {
                        let event = ResponseEvent::Completed {
                            response_id,
                            token_usage: usage.map(Into::into),
                        };
                        let _ = tx_event.send(Ok(event)).await;
                    }
                    None => {
                        let _ = tx_event
                            .send(Err(CodexErr::Stream(
                                "stream closed before response.completed".into(),
                            )))
                            .await;
                    }
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        let event: SseEvent = match serde_json::from_str(&sse.data) {
            Ok(event) => event,
            Err(e) => {
                debug!("Failed to parse SSE event: {e}, data: {}", &sse.data);
                continue;
            }
        };

        trace!(?event, "SSE event");
        match event.kind.as_str() {
            // Individual output item finalised. Forward immediately so the
            // rest of the agent can stream assistant text/functions *live*
            // instead of waiting for the final `response.completed` envelope.
            //
            // IMPORTANT: We used to ignore these events and forward the
            // duplicated `output` array embedded in the `response.completed`
            // payload.  That produced two concrete issues:
            //   1. No real‑time streaming – the user only saw output after the
            //      entire turn had finished, which broke the “typing” UX and
            //      made long‑running turns look stalled.
            //   2. Duplicate `function_call_output` items – both the
            //      individual *and* the completed array were forwarded, which
            //      confused the backend and triggered 400
            //      "previous_response_not_found" errors because the duplicated
            //      IDs did not match the incremental turn chain.
            //
            // The fix is to forward the incremental events *as they come* and
            // drop the duplicated list inside `response.completed`.
            "response.output_item.done" => {
                let Some(item_val) = event.item else { continue };
                let Ok(item) = serde_json::from_value::<ResponseItem>(item_val) else {
                    debug!("failed to parse ResponseItem from output_item.done");
                    continue;
                };

                let event = ResponseEvent::OutputItemDone(item);
                if tx_event.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            "response.created" => {
                if event.response.is_some() {
                    let _ = tx_event.send(Ok(ResponseEvent::Created {})).await;
                }
            }
            // Final response completed – includes array of output items & id
            "response.completed" => {
                if let Some(resp_val) = event.response {
                    match serde_json::from_value::<ResponseCompleted>(resp_val) {
                        Ok(r) => {
                            response_completed = Some(r);
                        }
                        Err(e) => {
                            debug!("failed to parse ResponseCompleted: {e}");
                            continue;
                        }
                    };
                };
            }
            "response.content_part.done"
            | "response.function_call_arguments.delta"
            | "response.in_progress"
            | "response.output_item.added"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {
                // Currently, we ignore these events, but we handle them
                // separately to skip the logging message in the `other` case.
            }
            other => debug!(other, "sse event"),
        }
    }
}

/// used in tests to stream from a text SSE file
async fn stream_from_fixture(path: impl AsRef<Path>) -> Result<ResponseStream> {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);
    let f = std::fs::File::open(path.as_ref())?;
    let lines = std::io::BufReader::new(f).lines();

    // insert \n\n after each line for proper SSE parsing
    let mut content = String::new();
    for line in lines {
        content.push_str(&line?);
        content.push_str("\n\n");
    }

    let rdr = std::io::Cursor::new(content);
    let stream = ReaderStream::new(rdr).map_err(CodexErr::Io);
    tokio::spawn(process_sse(stream, tx_event));
    Ok(ResponseStream { rx_event })
}
