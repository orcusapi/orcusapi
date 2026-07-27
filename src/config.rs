use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub rpc_url: String,
    pub network_passphrase: String,
    pub bind_addr: String,
    /// Optional Horizon-compatible RPC used for nothing right now; reserved.
    pub request_timeout_secs: u64,
    /// The contract WASM hash this instance of the proxy serves. Fixed at
    /// startup rather than taken per-request, since one proxy instance is
    /// meant to expose one contract's interface as an API.
    pub contract_wasm_hash: String,
    /// The deployed contract instance (C... strkey) that `/api` calls
    /// are sent to. Fixed at startup for the same reason as the WASM hash.
    pub contract_id: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Load a local .env if present; ignore if missing.
        let _ = dotenvy::dotenv();

        let rpc_url = std::env::var("SOROBAN_RPC_URL")
            .context("SOROBAN_RPC_URL must be set (e.g. https://soroban-testnet.stellar.org)")?;
        let network_passphrase = std::env::var("SOROBAN_NETWORK_PASSPHRASE").context(
            "SOROBAN_NETWORK_PASSPHRASE must be set (e.g. 'Test SDF Network ; September 2015')",
        )?;
        let bind_addr =
            std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let request_timeout_secs = std::env::var("REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let contract_wasm_hash = std::env::var("CONTRACT_WASM_HASH")
            .context("CONTRACT_WASM_HASH must be set to the hex-encoded WASM hash to serve")?
            .trim()
            .to_lowercase();
        crate::contract_source::parse_wasm_hash(&contract_wasm_hash)
            .context("CONTRACT_WASM_HASH is invalid")?;
        let contract_id = std::env::var("CONTRACT_ID")
            .context("CONTRACT_ID must be set to the deployed contract's C... address")?
            .trim()
            .to_string();
        stellar_strkey::Contract::from_string(&contract_id)
            .map_err(|_| anyhow::anyhow!("CONTRACT_ID is invalid: expected a C... strkey"))?;

        Ok(Self {
            rpc_url,
            network_passphrase,
            bind_addr,
            request_timeout_secs,
            contract_wasm_hash,
            contract_id,
        })
    }
}
