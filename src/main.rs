mod config;
mod contract_source;
mod error;
mod handlers;
mod probe;
mod rpc;
mod scval;
mod spec;
mod state;
mod txbuild;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use config::Config;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.request_timeout_secs))
        .build()?;

    let state = Arc::new(AppState::new(&config, http));

    let spec = contract_source::get_or_fetch_spec(
        &state.spec_cache,
        &state.rpc,
        &state.contract_wasm_hash,
    )
    .await?;
    tracing::info!(
        functions = spec.functions.len(),
        "probing functions to classify GET (read-only) vs POST (state-changing)..."
    );
    let methods = probe::classify_functions(&state.rpc, &state.contract_id, &spec).await;
    let get_count = methods.values().filter(|read_only| **read_only).count();
    tracing::info!(
        get = get_count,
        post = methods.len() - get_count,
        "function classification complete"
    );
    *state.function_methods.write().await = methods;

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/network", get(handlers::get_network))
        .route("/spec", get(handlers::get_spec))
        .route("/functions", get(handlers::get_functions))
        .route(
            "/api/{function_name}",
            get(handlers::invoke_get).post(handlers::invoke_post),
        )
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(
        addr = %config.bind_addr,
        rpc = %config.rpc_url,
        wasm_hash = %config.contract_wasm_hash,
        contract_id = %config.contract_id,
        "soroban-api-proxy listening"
    );
    axum::serve(listener, app).await?;

    Ok(())
}
