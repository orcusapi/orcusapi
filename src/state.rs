use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::rpc::RpcClient;
use crate::spec::ContractSpec;

pub struct AppState {
    pub rpc: RpcClient,
    pub network_passphrase: String,
    pub contract_wasm_hash: String,
    pub contract_id: String,
    pub spec_cache: RwLock<HashMap<String, Arc<ContractSpec>>>,
    /// Per-function classification, computed once at startup: `true` if the
    /// function should be called via `GET` (it takes no arguments and its
    /// simulated footprint has no writes), `false` if it should be called
    /// via `POST` (it takes arguments and/or changes contract state).
    /// Populated by `probe::classify_functions` before the server starts
    /// accepting requests; empty until then.
    pub function_methods: RwLock<HashMap<String, bool>>,
}

impl AppState {
    pub fn new(config: &Config, http: reqwest::Client) -> Self {
        Self {
            rpc: RpcClient::new(config.rpc_url.clone(), http),
            network_passphrase: config.network_passphrase.clone(),
            contract_wasm_hash: config.contract_wasm_hash.clone(),
            contract_id: config.contract_id.clone(),
            spec_cache: RwLock::new(HashMap::new()),
            function_methods: RwLock::new(HashMap::new()),
        }
    }

    /// Should `function_name` be called via `GET`? Defaults to `false`
    /// (`POST`) if it hasn't been classified yet.
    pub async fn should_use_get(&self, function_name: &str) -> bool {
        self.function_methods
            .read()
            .await
            .get(function_name)
            .copied()
            .unwrap_or(false)
    }
}
