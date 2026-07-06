# Local MLX Model Scheduling

## Goal

Start a local MLX inference server automatically from Zed's model selector,
without requiring the user to manually start a separate process (Ollama / LM Studio).

```
Before:  Start Ollama/LM Studio → Start Zed → Select model in popup
After:   Start Zed → Select local MLX model in popup → Zed manages everything
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Zed                                                    │
│  ┌──────────────────┐  ┌──────────────────────────────┐ │
│  │ Model Selector   │─▶│ LanguageModelRegistry        │ │
│  └──────────────────┘  │ ├─ Anthropic                 │ │
│                        │ ├─ Ollama                    │ │
│                        │ ├─ LmStudio                  │ │
│                        │ └─ LocalMlxProvider  ← NEW   │ │
│                        └─────────────┬────────────────┘ │
│                                      │                   │
│  ┌───────────────────────────────────▼────────────────┐ │
│  │ ProcessManager (new)                               │ │
│  │  • Spawn: mlx_lm.server --model X --port {P} --host 127.0.0.1
│  │  • Health-check: GET /health → 200                 │ │
│  │  • Graceful shutdown: SIGTERM → SIGKILL            │ │
│  │  • Idle timeout: stop after 5 min inactivity       │ │
│  │  • Crash recovery: auto-restart with backoff       │ │
│  └───────────────────────┬────────────────────────────┘ │
│                          │ HTTP (127.0.0.1:{port})      │
└──────────────────────────┼──────────────────────────────┘
                           │
                   ┌───────▼──────────────────┐
                   │ mlx-llm serve             │
                   │ (OpenAI-compatible API)   │
                   └──────────────────────────┘
```

## New crate: `crates/local_mlx/`

### File structure

```
crates/local_mlx/
├── Cargo.toml
└── src/
    ├── local_mlx.rs           # Crate root, re-exports
    ├── process_manager.rs     # Child process lifecycle
    ├── local_mlx_provider.rs  # LanguageModelProvider impl
    ├── local_mlx_model.rs     # LanguageModel impl
    ├── model_discovery.rs     # /v1/models + HF cache scanning
    ├── request.rs             # Custom request with extra params
    └── config_view.rs         # Settings UI
```

### Dependencies

```toml
[dependencies]
anyhow = { workspace = true }
futures = { workspace = true }
gpui = { workspace = true }
http_client = { workspace = true }
language_model = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
settings = { workspace = true }
ui = { workspace = true }
util = { workspace = true }
```

---

## Implementation Steps

### Step 1: Crate scaffolding & settings

- Create `crates/local_mlx/` with `Cargo.toml`
- Add `local_mlx` to workspace `Cargo.toml`
- Define `LocalMlxSettings` struct
- Add `local_mlx` field to `AllLanguageModelSettings`
- Define `LocalMlxAvailableModel` with per-model params:
  - `name`, `display_name`, `max_tokens`
  - `enable_thinking: Option<bool>`
  - `repeat_penalty: Option<f32>`
  - `top_p: Option<f32>`
  - `top_k: Option<u32>`

### Step 2: ProcessManager

Port-finding: use `TcpListener::bind("127.0.0.1:0")` → extract port → pass to server via `{port}` placeholder.

States: `Stopped → Starting → HealthChecking → Running → Stopping → Stopped`

Health-check: `GET http://127.0.0.1:{port}/health` with exponential backoff (100ms → 200ms → 400ms → … → max 5s).

Idle timeout: configurable (default 300s). Background task checks `last_used` every 30s.

Crash: if child exits unexpectedly → emit event → auto-restart with backoff.

### Step 3: Model Discovery

1. Primary: `GET http://127.0.0.1:{port}/v1/models` → parse OpenAI-compatible response
2. Fallback: scan `~/.cache/huggingface/hub/` for `*.safetensors` + `config.json`

### Step 4: Custom Request

`LocalMlxRequest` with all standard OpenAI fields plus:
- `top_p`, `top_k`, `repeat_penalty`, `frequency_penalty`
- `extra_body: Option<ExtraBody>` for `enable_thinking`

Converted from `LanguageModelRequest` + per-model settings.

### Step 5: LocalMlxLanguageModel

Implements `LanguageModel` trait.
`stream_completion()`:
1. `process_manager.touch()` — reset idle timer
2. Get `server_port` from process manager (start if needed)
3. Build `LocalMlxRequest`
4. Stream HTTP response, parse SSE, emit `LanguageModelCompletionEvent`

### Step 6: LocalMlxLanguageModelProvider

Implements `LanguageModelProvider` + `LanguageModelProviderState`.
- `is_authenticated()` → server running + models discovered
- `authenticate()` → start server + fetch models
- `provided_models()` → list discovered models

### Step 7: Config UI

Simple view with:
- Status indicator (stopped / starting / running with port)
- Server command input (`uvx mlx-llm serve --model {model} --port {port}`)
- Per-model parameter overrides
- Start / Stop button
- Idle timeout slider

### Step 8: Registry Integration
---
## Final Status: ✅ Implemented

All steps implemented. `local_mlx` and `language_models` crates compile cleanly.

### Files Changed

**New:**
- `crates/local_mlx/Cargo.toml`
- `crates/local_mlx/src/local_mlx.rs`
- `crates/local_mlx/src/process_manager.rs` — spawn/stop/health-check/idle-watcher/Drop-cleanup
- `crates/local_mlx/src/request.rs` — custom OpenAI-compatible request with tools, sampling params
- `crates/local_mlx/src/model_discovery.rs` — /v1/models API + HF cache scan with config.json parsing
- `crates/language_models/src/provider/local_mlx.rs` — provider, model, config view
- `docs/local-mlx-usage.md` — user documentation

**Modified:**
- `Cargo.toml` — workspace member + dependency
- `crates/language_models/Cargo.toml` — local_mlx dependency
- `crates/language_models/src/language_models.rs` — provider registration
- `crates/language_models/src/provider.rs` — module declaration
- `crates/language_models/src/settings.rs` — LocalMlxSettings
- `crates/settings_content/src/language_model.rs` — settings content structs
- `.rules` — tool usage rule (find/replace over line numbers)

### Capabilities

| Feature | Status |
|---|---|
| Auto-start server on first model selection | ✅ |
| Auto-restart on model switch | ✅ |
| Idle timeout (configurable) | ✅ |
| Crash detection + recovery | ✅ |
| Orphan process cleanup (Drop) | ✅ |
| Tool calling (agent support) | ✅ |
| Custom sampling params (top_p, top_k, repeat_penalty) | ✅ |
| enable_thinking per model | ✅ |
| Context window from config.json | ✅ |
| HF cache model discovery | ✅ |
| Error visibility in config UI | ✅ |
| macOS bundle PATH handling | ✅ |
| External tool access (127.0.0.1) | ✅ |
