use std::path::{Path, PathBuf};

use clap::Parser;
use hf_hub::api::tokio::{Api, ApiBuilder};
use hf_hub::{Cache, Repo, RepoType};
use tracing::info;

use text_generation_backends_trtllm::errors::TensorRtLlmBackendError;
use text_generation_backends_trtllm::TensorRtLlmBackendV2;
use text_generation_router::server::{
    get_hub_model_info, legacy_tokenizer_handle, py_resolve_tokenizer,
};
use text_generation_router::usage_stats::UsageStatsLevel;
use text_generation_router::{server, Tokenizer};

/// App Configuration
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(default_value = "128", long, env)]
    max_concurrent_requests: usize,
    #[clap(default_value = "2", long, env)]
    max_best_of: usize,
    #[clap(default_value = "4", long, env)]
    max_stop_sequences: usize,
    #[clap(default_value = "5", long, env)]
    max_top_n_tokens: u32,
    #[clap(default_value = "1024", long, env)]
    max_input_tokens: usize,
    #[clap(default_value = "2048", long, env)]
    max_total_tokens: usize,
    #[clap(default_value = "4096", long, env)]
    max_batch_prefill_tokens: u32,
    #[clap(long, env)]
    max_batch_total_tokens: Option<u32>,
    #[clap(default_value = "0.0.0.0", long, env)]
    hostname: String,
    #[clap(default_value = "3000", long, short, env)]
    port: u16,
    #[clap(default_value = "9000", long, short, env)]
    prometheus_port: u16,
    #[clap(long, env, required = true)]
    tokenizer_name: String,
    #[clap(long, env)]
    tokenizer_config_path: Option<String>,
    #[clap(long, env)]
    revision: Option<String>,
    #[clap(long, env)]
    model_id: String,
    #[clap(default_value = "2", long, env)]
    validation_workers: usize,
    #[clap(long, env)]
    json_output: bool,
    #[clap(long, env)]
    otlp_endpoint: Option<String>,
    #[clap(default_value = "text-generation-inference.router", long, env)]
    otlp_service_name: String,
    #[clap(long, env)]
    cors_allow_origin: Option<Vec<String>>,
    #[clap(default_value = "4", long, env)]
    max_client_batch_size: usize,
    #[clap(long, env)]
    auth_token: Option<String>,
    #[clap(long, env, help = "Path to the TensorRT-LLM Orchestrator worker")]
    executor_worker: PathBuf,
    #[clap(default_value = "on", long, env)]
    usage_stats: UsageStatsLevel,
    #[clap(default_value = "2000000", long, env)]
    payload_limit: usize,
}

async fn get_tokenizer(tokenizer_name: &str, revision: Option<&str>) -> Option<Tokenizer> {
    // Parse Huggingface hub token
    let authorization_token = std::env::var("HF_TOKEN")
        .or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN"))
        .ok();

    // Tokenizer instance
    let local_path = Path::new(tokenizer_name);

    // Shared API builder initialization
    let api_builder = || {
        let mut builder = ApiBuilder::new()
            .with_progress(false)
            .with_token(authorization_token);

        if let Ok(cache_dir) = std::env::var("HUGGINGFACE_HUB_CACHE") {
            builder = builder.with_cache_dir(cache_dir.into());
        }

        if let Ok(origin) = std::env::var("HF_HUB_USER_AGENT_ORIGIN") {
            builder = builder.with_user_agent("origin", origin.as_str());
        }

        builder
    };

    // Decide if we need to use the API based on the revision and local path
    let use_api = revision.is_some() || !local_path.exists() || !local_path.is_dir();

    // Initialize API if needed
    #[derive(Clone)]
    enum Type {
        Api(Api),
        Cache(Cache),
        None,
    }
    let api = if use_api {
        if std::env::var("HF_HUB_OFFLINE") == Ok("1".to_string()) {
            let cache = std::env::var("HUGGINGFACE_HUB_CACHE")
                .map_err(|_| ())
                .map(|cache_dir| Cache::new(cache_dir.into()))
                .unwrap_or_else(|_| Cache::default());
            tracing::warn!("Offline mode active using cache defaults");
            Type::Cache(cache)
        } else {
            tracing::info!("Using the Hugging Face API");
            match api_builder().build() {
                Ok(api) => Type::Api(api),
                Err(_) => {
                    tracing::warn!("Unable to build the Hugging Face API");
                    Type::None
                }
            }
        }
    } else {
        Type::None
    };

    // Load tokenizer and model info
    let (
        config_filename,
        _tokenizer_config_filename,
        _preprocessor_config_filename,
        _processor_config_filename,
        _model_info,
    ) = match api {
        Type::None => (
            Some(local_path.join("config.json")),
            Some(local_path.join("tokenizer_config.json")),
            Some(local_path.join("preprocessor_config.json")),
            Some(local_path.join("processor_config.json")),
            None,
        ),
        Type::Api(api) => {
            let api_repo = api.repo(Repo::with_revision(
                tokenizer_name.to_string(),
                RepoType::Model,
                revision.unwrap_or_else(|| "main").to_string(),
            ));

            let config_filename = api_repo.get("config.json").await.ok();
            let tokenizer_config_filename = api_repo.get("tokenizer_config.json").await.ok();
            let preprocessor_config_filename = api_repo.get("preprocessor_config.json").await.ok();
            let processor_config_filename = api_repo.get("processor_config.json").await.ok();

            let model_info = if let Some(model_info) = get_hub_model_info(&api_repo).await {
                Some(model_info)
            } else {
                tracing::warn!("Could not retrieve model info from the Hugging Face hub.");
                None
            };
            (
                config_filename,
                tokenizer_config_filename,
                preprocessor_config_filename,
                processor_config_filename,
                model_info,
            )
        }
        Type::Cache(cache) => {
            let repo = cache.repo(Repo::with_revision(
                tokenizer_name.to_string(),
                RepoType::Model,
                revision.clone().unwrap_or_else(|| "main").to_string(),
            ));
            (
                repo.get("config.json"),
                repo.get("tokenizer_config.json"),
                repo.get("preprocessor_config.json"),
                repo.get("processor_config.json"),
                None,
            )
        }
    };

    let tokenizer: Tokenizer = {
        use pyo3::prelude::*;
        pyo3::Python::with_gil(|py| -> PyResult<()> {
            py_resolve_tokenizer(py, &tokenizer_name, revision.as_deref(), false)?;
            Ok(())
        })
        .inspect_err(|err| {
            tracing::error!("Failed to import python tokenizer {err}");
        })
        .or_else(|err| {
            let out = legacy_tokenizer_handle(config_filename.as_ref());
            out.ok_or(err)
        })
        .expect("We cannot load a tokenizer");
        let filename = "out/tokenizer.json";
        if let Ok(tok) = tokenizers::Tokenizer::from_file(filename) {
            Tokenizer::Rust(tok)
        } else {
            Tokenizer::Python {
                tokenizer_name: tokenizer_name.to_string(),
                revision: revision.map(|revision| revision.to_string()),
                trust_remote_code: false,
            }
        }
    };

    Some(tokenizer)
}

#[tokio::main]
async fn main() -> Result<(), TensorRtLlmBackendError> {
    // Get args
    let args = Args::parse();
    // Pattern match configuration
    let Args {
        max_concurrent_requests,
        max_best_of,
        max_stop_sequences,
        max_top_n_tokens,
        max_input_tokens,
        max_total_tokens,
        max_batch_prefill_tokens,
        max_batch_total_tokens,
        hostname,
        port,
        prometheus_port,
        tokenizer_name,
        tokenizer_config_path,
        revision,
        model_id,
        validation_workers,
        json_output,
        otlp_endpoint,
        otlp_service_name,
        cors_allow_origin,
        max_client_batch_size,
        auth_token,
        executor_worker,
        usage_stats,
        payload_limit,
    } = args;

    // Launch Tokio runtime
    text_generation_router::logging::init_logging(otlp_endpoint, otlp_service_name, json_output);

    // Validate args
    if max_input_tokens >= max_total_tokens {
        return Err(TensorRtLlmBackendError::ArgumentValidation(
            "`max_input_tokens` must be < `max_total_tokens`".to_string(),
        ));
    }
    if max_input_tokens as u32 > max_batch_prefill_tokens {
        return Err(TensorRtLlmBackendError::ArgumentValidation(format!("`max_batch_prefill_tokens` must be >= `max_input_tokens`. Given: {max_batch_prefill_tokens} and {max_input_tokens}")));
    }

    if validation_workers == 0 {
        return Err(TensorRtLlmBackendError::ArgumentValidation(
            "`validation_workers` must be > 0".to_string(),
        ));
    }

    if let Some(ref max_batch_total_tokens) = max_batch_total_tokens {
        if max_batch_prefill_tokens > *max_batch_total_tokens {
            return Err(TensorRtLlmBackendError::ArgumentValidation(format!("`max_batch_prefill_tokens` must be <= `max_batch_total_tokens`. Given: {max_batch_prefill_tokens} and {max_batch_total_tokens}")));
        }
        if max_total_tokens as u32 > *max_batch_total_tokens {
            return Err(TensorRtLlmBackendError::ArgumentValidation(format!("`max_total_tokens` must be <= `max_batch_total_tokens`. Given: {max_total_tokens} and {max_batch_total_tokens}")));
        }
    }

    if !executor_worker.exists() {
        return Err(TensorRtLlmBackendError::ArgumentValidation(format!(
            "`executor_work` specified path doesn't exists: {}",
            executor_worker.display()
        )));
    }

    // Create the backend
    match get_tokenizer(&tokenizer_name, revision.as_deref())
        .await
        .expect("Failed to retrieve tokenizer implementation")
    {
        Tokenizer::Python { .. } => Err(TensorRtLlmBackendError::Tokenizer(
            "Failed to retrieve Rust based tokenizer".to_string(),
        )),
        Tokenizer::Rust(tokenizer) => {
            info!("Successfully retrieved tokenizer {}", &tokenizer_name);
            let backend = TensorRtLlmBackendV2::new(
                tokenizer,
                model_id,
                executor_worker,
                max_concurrent_requests,
            )?;

            info!("Successfully created backend");

            // Run server
            server::run(
                backend,
                max_concurrent_requests,
                max_best_of,
                max_stop_sequences,
                max_top_n_tokens,
                max_input_tokens,
                max_total_tokens,
                validation_workers,
                auth_token,
                tokenizer_name,
                tokenizer_config_path,
                revision,
                false,
                hostname,
                port,
                cors_allow_origin,
                false,
                None,
                None,
                true,
                max_client_batch_size,
                usage_stats,
                payload_limit,
                prometheus_port,
            )
            .await?;
            Ok(())
        }
    }
}
