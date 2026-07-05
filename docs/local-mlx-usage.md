# Local MLX Models in Zed

Run local MLX-accelerated models (Qwen3-Coder, Llama, DeepSeek, etc.) directly from Zed's model selector — no separate server process to manage.

## Prerequisites

1. **macOS with Apple Silicon** (M1/M2/M3/M4)
2. **`uv`** installed: `curl -LsSf https://astral.sh/uv/install.sh | sh`
3. **`mlx-llm`** (auto-installed by `uvx` on first run)

## Quick Start

Add to `~/.config/zed/settings.json`:

```json
{
  "language_models": {
    "local_mlx": {
      "available_models": [
        {
          "name": "mlx-community/Qwen3-Coder-Next-6bit",
          "display_name": "Qwen3 Coder 6bit",
          "max_tokens": 32768,
          "enable_thinking": false,
          "repeat_penalty": 1.1
        },
        {
          "name": "mlx-community/Qwen3-Coder-Next-8bit",
          "display_name": "Qwen3 Coder 8bit",
          "max_tokens": 32768,
          "enable_thinking": false
        }
      ]
    }
  }
}
```

Start Zed, open the agent panel, and "Local MLX" appears in the model selector.

## How It Works

```
Zed starts
  → Registers "Local MLX" provider
  → Spawns: uvx mlx-llm serve --model <first-model> --port <auto>
  → Health-checks via TCP connect
  → Models appear in selector popup

You select a model
  → Zed checks if server is running with the right model
  → If different model: auto-restarts server with new model
  → If idle > 5 min: auto-stops server (configurable)

You type a message
  → Request sent to http://127.0.0.1:{port}/v1/chat/completions
  → Response streamed back via SSE
```

## Configuration Reference

### `server_binary`

Default: `"uvx"`. Change to full path if `uvx` is not in PATH:
```json
"server_binary": "/Users/you/.local/bin/uvx"
```

### `server_args`

Default: `["mlx-llm", "serve", "--model", "{model}", "--port", "{port}"]`

Placeholders:
- `{model}` — replaced with the selected model name
- `{port}` — replaced with an auto-assigned free port

### `idle_timeout_seconds`

Default: `300` (5 minutes). Set to `0` to disable auto-stop.

### `available_models`

Array of model configurations:

| Field | Required | Description |
|---|---|---|
| `name` | yes | HuggingFace model ID, e.g. `"mlx-community/Qwen3-Coder-Next-6bit"` |
| `display_name` | no | Name shown in Zed's model selector |
| `max_tokens` | yes | Context window size |
| `enable_thinking` | no | Set `false` for Qwen3 coder models to disable thinking mode |
| `repeat_penalty` | no | Penalty for token repetition (e.g. `1.1`) |
| `top_p` | no | Nucleus sampling (e.g. `0.95`) |
| `top_k` | no | Top-k sampling |

## Model Discovery

Models can also be auto-discovered from your HuggingFace cache
(`~/.cache/huggingface/hub/`). If models are downloaded there, they appear
automatically in the selector alongside configured models.

For auto-discovered models, the context window is read from `config.json`
(`max_position_embeddings` field).

## Server Status

Open **Settings → Language Models → Local MLX** to see:
- Server status (Running/Starting/Stopped)
- Current port and loaded model
- Error messages (e.g. `uvx` not found)

## Limitations

- **No tool calling yet** — the model can chat but cannot use agent tools
- **Single model at a time** — switching models restarts the server (~2-3s delay)
- **Localhost only** — not accessible from other devices
- **macOS only** — MLX requires Apple Silicon

## Troubleshooting

### "Local MLX" doesn't appear in model selector

Check Settings → Language Models → Local MLX for errors.

### `uvx: command not found`

Install `uv`: https://docs.astral.sh/uv/getting-started/installation/

Or use full path in `server_binary`.

### Server starts but requests fail

Check that the model is downloaded:
```sh
uvx mlx-llm download mlx-community/Qwen3-Coder-Next-6bit
```

### Port already in use

Zed auto-assigns a free port. If you see port conflicts, stop any other
`mlx-llm serve` processes: `pkill -f "mlx-llm serve"`
