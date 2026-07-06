use anyhow::{Context, Result};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub max_tokens: u64,
    pub supports_tools: bool,
    pub supports_images: bool,
    pub local_path: Option<PathBuf>,
}

/// Discover available models from the running mlx-llm server.
pub async fn discover_models_via_api(
    http_client: &dyn HttpClient,
    api_base: &str,
) -> Result<Vec<ModelInfo>> {
    let url = format!("{}/v1/models", api_base);

    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("Accept", "application/json")
        .body(AsyncBody::empty())?;

    let response = http_client.send(request).await?;
    let status = response.status();

    if !status.is_success() {
        let mut body = String::new();
        response.into_body().read_to_string(&mut body).await?;
        return Err(anyhow::anyhow!(
            "Failed to list models: HTTP {} - {}",
            status,
            body
        ));
    }

    let mut body = String::new();
    response.into_body().read_to_string(&mut body).await?;

    #[derive(serde::Deserialize)]
    struct ModelsResponse {
        data: Vec<ModelEntry>,
    }

    #[derive(serde::Deserialize)]
    struct ModelEntry {
        id: String,
    }

    let models_response: ModelsResponse =
        serde_json::from_str(&body).context("Failed to parse /v1/models response")?;

    let models = models_response
        .data
        .into_iter()
        .map(|entry| {
            let display_name = entry
                .id
                .rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(&entry.id)
                .to_string();

            ModelInfo {
                id: entry.id,
                display_name,
                max_tokens: 32768,
                supports_tools: true,
                supports_images: false,
                local_path: None,
            }
        })
        .collect();

    Ok(models)
}

/// Scan the HuggingFace cache for locally downloaded MLX models.
/// Parses config.json to extract the real context window size.
pub fn discover_models_from_cache() -> Result<Vec<ModelInfo>> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("Could not determine home directory")?;

    let cache_dir = std::path::PathBuf::from(home)
        .join(".cache")
        .join("huggingface")
        .join("hub");

    if !cache_dir.exists() {
        return Ok(Vec::new());
    }

    let mut models = Vec::new();

    let entries = std::fs::read_dir(&cache_dir)
        .with_context(|| format!("Failed to read cache directory: {}", cache_dir.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let dir_name = entry.file_name().to_string_lossy().to_string();

        if dir_name.starts_with("models--") {
            let snapshots_dir = entry.path().join("snapshots");
            if !snapshots_dir.exists() {
                continue;
            }

            // Find the first snapshot that has both config.json and model weights
            let (config_path, safetensors_path) = find_model_files(&snapshots_dir);

            // Require at least one safetensors file — config.json alone means
            // the model was never fully downloaded.
            let has_model = safetensors_path.is_some();

            // Read config.json for model metadata
            let max_tokens = config_path
                .as_ref()
                .and_then(|p| read_context_length(p).ok())
                .unwrap_or(32768);

            if has_model {
                let id = dir_name
                    .strip_prefix("models--")
                    .unwrap_or(&dir_name)
                    .replace("--", "/");

                let display_name = id
                    .rsplit_once('/')
                    .map(|(_, n)| n)
                    .unwrap_or(&id)
                    .to_string();

                let local_path = safetensors_path
                    .as_ref()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()));

                models.push(ModelInfo {
                    id,
                    display_name,
                    max_tokens,
                    supports_tools: true,
                    supports_images: false,
                    local_path,
                });
            }
        }
    }

    Ok(models)
}

fn find_model_files(
    snapshots_dir: &Path,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(snapshots_dir) {
        Ok(entries) => entries,
        Err(_) => return (None, None),
    };
    let mut config_path = None;
    let mut safetensors_path = None;
    for entry in entries.filter_map(|e| e.ok()) {
        if config_path.is_none() {
            let config = entry.path().join("config.json");
            if config.exists() {
                config_path = Some(config);
            }
        }
        if safetensors_path.is_none() {
            if let Ok(files) = std::fs::read_dir(entry.path()) {
                for file in files.filter_map(|f| f.ok()) {
                    let name = file.file_name().to_string_lossy().to_string();
                    if name.ends_with(".safetensors") {
                        safetensors_path = Some(file.path());
                        break;
                    }
                }
            }
        }
        if config_path.is_some() && safetensors_path.is_some() {
            break;
        }
    }
    (config_path, safetensors_path)
}

fn read_context_length(config_path: &Path) -> Result<u64> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let config: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse config.json")?;

    // Try several known field names for context length:
    // Qwen3: max_position_embeddings
    // Llama: max_position_embeddings
    // Mistral: max_position_embeddings
    // DeepSeek: max_position_embeddings
    // Phi: max_position_embeddings
    if let Some(v) = config["max_position_embeddings"].as_u64() {
        return Ok(v);
    }

    // Some models use model_max_length in tokenizer_config
    // Others use n_positions
    if let Some(v) = config["n_positions"].as_u64() {
        return Ok(v);
    }

    // text-generation models sometimes have this
    if let Some(v) = config["max_seq_length"].as_u64() {
        return Ok(v);
    }

    // Fallback to a sensible default
    Ok(32768)
}
