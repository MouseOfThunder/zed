use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use futures::{
    AsyncBufReadExt, AsyncReadExt, FutureExt, StreamExt, io::BufReader, stream::BoxStream,
};
use gpui::{
    AnyView, App, AppContext, AsyncApp, Context, Entity, ParentElement, Styled, Task, Window,
};
use http_client::{HttpClient, Request};
use language_model::{
    AuthenticateError, LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent,
    LanguageModelId, LanguageModelName, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState, LanguageModelRequest,
    LanguageModelToolChoice, LanguageModelToolUse, MessageContent, RateLimiter, StopReason,
};
use local_mlx::{LocalMlxRequest, ModelInfo, ProcessManager};
use settings::Settings as _;
use std::sync::Arc;
use std::time::Duration;

use crate::AllLanguageModelSettings;

pub use settings::LocalMlxAvailableModel as AvailableModel;

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("local-mlx");
const PROVIDER_NAME: LanguageModelProviderName = LanguageModelProviderName::new("Local MLX");

#[derive(Default, Debug, Clone, PartialEq)]
pub struct LocalMlxSettings {
    pub server_binary: String,
    pub server_args: Vec<String>,
    pub model_directory: Option<std::path::PathBuf>,
    pub idle_timeout_seconds: u64,
    pub available_models: Vec<AvailableModel>,
}

pub struct LocalMlxLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    process_manager: Arc<ProcessManager>,
    state: Entity<State>,
}

pub struct State {
    process_manager: Arc<ProcessManager>,
    http_client: Arc<dyn HttpClient>,
    discovered_models: Vec<ModelInfo>,
    server_port: Option<u16>,
    server_binary: String,
    server_args: Vec<String>,
    idle_timeout_seconds: u64,
    last_error: Option<String>,
    _idle_watcher_task: Option<Task<()>>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        true
    }

    fn first_model_from_settings(&self, cx: &App) -> Option<String> {
        AllLanguageModelSettings::get_global(cx)
            .local_mlx
            .available_models
            .first()
            .map(|m| m.name.clone())
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        if self.process_manager.is_running() {
            return Task::ready(Ok(()));
        }

        let Some(model_name) = self.first_model_from_settings(cx) else {
            return Task::ready(Err(AuthenticateError::CredentialsNotFound));
        };

        self.start_server_for_model(model_name, cx)
    }

    fn start_server_for_model(
        &mut self,
        model_name: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), AuthenticateError>> {
        let process_manager = self.process_manager.clone();
        let server_binary = self.server_binary.clone();
        let server_args = self.server_args.clone();
        let idle_timeout = Duration::from_secs(self.idle_timeout_seconds);

        cx.spawn(async move |this, cx| {
            let port = process_manager
                .start(
                    &server_binary,
                    &server_args,
                    &model_name,
                    cx.background_executor(),
                )
                .await
                .map_err(|err| {
                    let msg = format!("Failed to start local MLX server: {}", err);
                    log::warn!("{}", msg);
                    AuthenticateError::Other(err)
                })?;

            let idle_watcher = if idle_timeout.as_secs() > 0 {
                Some(process_manager.spawn_idle_watcher(idle_timeout, cx.background_executor()))
            } else {
                None
            };

            this.update(cx, |this, cx| {
                this.server_port = Some(port);
                this.last_error = None;
                this._idle_watcher_task = idle_watcher;
                cx.notify();
            })?;

            Ok(())
        })
    }
}

impl LocalMlxLanguageModelProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, cx: &mut App) -> Arc<Self> {
        let process_manager = Arc::new(ProcessManager::new());

        Arc::new(Self {
            http_client: http_client.clone(),
            process_manager: process_manager.clone(),
            state: cx.new(|cx| {
                let settings = AllLanguageModelSettings::get_global(cx).local_mlx.clone();
                State {
                    process_manager: process_manager.clone(),
                    http_client: http_client.clone(),
                    discovered_models: Vec::new(),
                    server_port: None,
                    server_binary: settings.server_binary,
                    server_args: settings.server_args,
                    idle_timeout_seconds: settings.idle_timeout_seconds,
                    last_error: None,
                    _idle_watcher_task: None,
                }
            }),
        })
    }
}

impl LanguageModelProvider for LocalMlxLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        self.provided_models(cx).into_iter().next()
    }

    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        self.default_model(cx)
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let settings = AllLanguageModelSettings::get_global(cx).local_mlx.clone();
        let discovered = self.state.read(cx).discovered_models.clone();
        let mut models: Vec<Arc<dyn LanguageModel>> = Vec::new();

        for config_model in &settings.available_models {
            let model_info = ModelInfo {
                id: config_model.name.clone(),
                display_name: config_model
                    .display_name
                    .clone()
                    .unwrap_or_else(|| config_model.name.clone()),
                max_tokens: config_model.max_tokens,
                supports_tools: true,
                supports_images: false,
            };

            models.push(Arc::new(LocalMlxLanguageModel::new(
                model_info,
                self.http_client.clone(),
                self.process_manager.clone(),
                self.state.read(cx).server_binary.clone(),
                self.state.read(cx).server_args.clone(),
                config_model.enable_thinking,
                config_model.repeat_penalty,
                config_model.top_p,
                config_model.top_k,
            )));
        }

        for discovered_model in &discovered {
            if !settings
                .available_models
                .iter()
                .any(|m| m.name == discovered_model.id)
            {
                models.push(Arc::new(LocalMlxLanguageModel::new(
                    discovered_model.clone(),
                    self.http_client.clone(),
                    self.process_manager.clone(),
                    self.state.read(cx).server_binary.clone(),
                    self.state.read(cx).server_args.clone(),
                    None,
                    None,
                    None,
                    None,
                )));
            }
        }

        models
    }

    fn recommended_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        self.provided_models(cx)
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn configuration_view(
        &self,
        _target_agent: language_model::ConfigurationViewTargetAgent,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        cx.new(|cx| ConfigurationView::new(self.state.clone(), window, cx))
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        self.process_manager.stop(cx.background_executor())
    }
}

impl LanguageModelProviderState for LocalMlxLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

// --- Configuration View ---

struct ConfigurationView {
    state: Entity<State>,
}

impl ConfigurationView {
    fn new(state: Entity<State>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self { state }
    }
}

impl gpui::Render for ConfigurationView {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let state = self.state.read(cx);
        let is_running = state.process_manager.is_running();
        let is_starting = state.process_manager.is_starting();
        let port = state.process_manager.port();
        let current_model = state.process_manager.current_model();
        let binary = state.server_binary.clone();
        let discovered = state.discovered_models.len();
        let last_error = state.last_error.clone();
        drop(state);

        let status = if is_running {
            format!(
                "Running on port {} (model: {})",
                port.unwrap_or(0),
                current_model.as_deref().unwrap_or("unknown")
            )
        } else if is_starting {
            "Starting...".to_string()
        } else {
            "Stopped".to_string()
        };

        let mut div = gpui::div()
            .child("Local MLX Server")
            .child(format!("Status: {}", status))
            .child(format!(
                "Command: {} {}",
                binary,
                self.state.read(cx).server_args.join(" ")
            ))
            .child(format!("Models discovered: {}", discovered));

        if let Some(err) = last_error {
            div = div.child(
                gpui::div()
                    .text_color(gpui::red())
                    .child(format!("Error: {}", err)),
            );
        }

        div.child("Configure models in settings.json → language_models.local_mlx.available_models")
    }
}

pub struct LocalMlxLanguageModel {
    id: LanguageModelId,
    model_info: ModelInfo,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
    process_manager: Arc<ProcessManager>,
    server_binary: String,
    server_args: Vec<String>,
    enable_thinking: Option<bool>,
    repeat_penalty: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<u32>,
}

impl LocalMlxLanguageModel {
    pub fn new(
        model_info: ModelInfo,
        http_client: Arc<dyn HttpClient>,
        process_manager: Arc<ProcessManager>,
        server_binary: String,
        server_args: Vec<String>,
        enable_thinking: Option<bool>,
        repeat_penalty: Option<f32>,
        top_p: Option<f32>,
        top_k: Option<u32>,
    ) -> Self {
        Self {
            id: LanguageModelId::from(model_info.id.clone()),
            model_info,
            http_client,
            request_limiter: RateLimiter::new(1),
            process_manager,
            server_binary,
            server_args,
            enable_thinking,
            repeat_penalty,
            top_p,
            top_k,
        }
    }
}

impl LanguageModel for LocalMlxLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model_info.display_name.clone())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn telemetry_id(&self) -> String {
        format!("local-mlx/{}", self.model_info.id)
    }

    fn supports_tools(&self) -> bool {
        self.model_info.supports_tools
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        matches!(
            choice,
            LanguageModelToolChoice::Auto
                | LanguageModelToolChoice::Any
                | LanguageModelToolChoice::None
        )
    }

    fn supports_images(&self) -> bool {
        self.model_info.supports_images
    }

    fn max_token_count(&self) -> u64 {
        self.model_info.max_tokens
    }

    fn max_output_tokens(&self) -> Option<u64> {
        None
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let http_client = self.http_client.clone();
        let model_name = self.model_info.id.clone();
        let enable_thinking = self.enable_thinking;
        let repeat_penalty = self.repeat_penalty;
        let top_p = self.top_p;
        let top_k = self.top_k;
        let request_limiter = self.request_limiter.clone();
        let process_manager = self.process_manager.clone();
        let server_binary = self.server_binary.clone();
        let server_args = self.server_args.clone();
        let executor = cx.background_executor().clone();

        let future = request_limiter.stream(async move {
            process_manager
                .ensure_model(&server_binary, &server_args, &model_name, &executor)
                .await
                .map_err(|e| {
                    LanguageModelCompletionError::Other(anyhow!(
                        "Failed to start local MLX server: {}",
                        e
                    ))
                })?;

            process_manager.touch();

            let api_url = format!(
                "http://127.0.0.1:{}/v1",
                process_manager.port().unwrap_or(0)
            );

            let local_request = LocalMlxRequest::from_language_model_request(
                request,
                &model_name,
                enable_thinking,
                repeat_penalty,
                top_p,
                top_k,
            );

            let body = serde_json::to_string(&local_request)
                .map_err(|e| LanguageModelCompletionError::Other(e.into()))?;

            let http_request = Request::builder()
                .method(http_client::Method::POST)
                .uri(format!("{}/chat/completions", api_url))
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .body(http_client::AsyncBody::from(body))
                .map_err(|e| LanguageModelCompletionError::Other(e.into()))?;

            let response = http_client
                .send(http_request)
                .await
                .map_err(|e| LanguageModelCompletionError::Other(e))?;

            let status = response.status();
            if !status.is_success() {
                let mut body = String::new();
                let _ = response.into_body().read_to_string(&mut body).await;
                return Err(LanguageModelCompletionError::Other(anyhow!(
                    "Local MLX API error: HTTP {} - {}",
                    status,
                    body
                )));
            }

            let reader = BufReader::new(response.into_body());
            let stream = reader
                .lines()
                .filter_map(|line| {
                    std::future::ready(match line {
                        Ok(line) if line.starts_with("data: ") => {
                            let data = line["data: ".len()..].to_string();
                            if data == "[DONE]" {
                                None
                            } else {
                                Some(parse_chunk(&data))
                            }
                        }
                        Ok(_) => None,
                        Err(e) => Some(Err(LanguageModelCompletionError::Other(anyhow!(
                            "Stream error: {}",
                            e
                        )))),
                    })
                })
                .boxed();

            Ok(stream)
        });

        async move { Ok(future.await?.boxed()) }.boxed()
    }
}

fn parse_chunk(data: &str) -> Result<LanguageModelCompletionEvent, LanguageModelCompletionError> {
    #[derive(serde::Deserialize)]
    struct Chunk {
        choices: Vec<ChoiceDelta>,
    }

    #[derive(serde::Deserialize)]
    struct ChoiceDelta {
        delta: Delta,
        finish_reason: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct Delta {
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCallDelta>,
    }

    #[derive(serde::Deserialize)]
    struct ToolCallDelta {
        #[serde(default)]
        index: usize,
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "type", default)]
        call_type: Option<String>,
        function: Option<ToolCallFunc>,
    }

    #[derive(serde::Deserialize)]
    struct ToolCallFunc {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    }

    let chunk: Chunk =
        serde_json::from_str(data).map_err(|e| LanguageModelCompletionError::Other(e.into()))?;

    if let Some(choice) = chunk.choices.first() {
        if let Some(finish_reason) = &choice.finish_reason {
            return Ok(LanguageModelCompletionEvent::Stop(
                match finish_reason.as_str() {
                    "stop" => StopReason::EndTurn,
                    "length" => StopReason::MaxTokens,
                    "tool_calls" => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                },
            ));
        }

        for tc in &choice.delta.tool_calls {
            if let Some(ref func) = tc.function {
                if let (Some(name), Some(id)) = (&func.name, &tc.id) {
                    let arguments = func.arguments.clone().unwrap_or_default();
                    return Ok(LanguageModelCompletionEvent::ToolUse(
                        LanguageModelToolUse {
                            id: id.clone().into(),
                            name: name.clone().into(),
                            raw_input: arguments.clone(),
                            input: serde_json::from_str(&arguments).unwrap_or_default(),
                            is_input_complete: true,
                            thought_signature: None,
                        },
                    ));
                }
            }
        }

        if let Some(content) = &choice.delta.content {
            if !content.is_empty() {
                return Ok(LanguageModelCompletionEvent::Text(content.clone()));
            }
        }
    }

    Ok(LanguageModelCompletionEvent::Text(String::new()))
}
