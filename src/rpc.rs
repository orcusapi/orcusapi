use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Minimal JSON-RPC 2.0 client for the Soroban / Stellar RPC service.
///
/// Only the handful of methods this proxy needs are implemented; anything
/// else is reachable via `call` directly.
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    next_id: AtomicU64,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

impl RpcClient {
    pub fn new(url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            url: url.into(),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("rpc transport error calling {method}: {e}"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("failed reading rpc response body for {method}: {e}"))?;

        if !status.is_success() {
            return Err(anyhow!(
                "rpc http error calling {method}: status={status} body={text}"
            ));
        }

        let parsed: JsonRpcResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("failed parsing rpc response for {method}: {e} body={text}"))?;

        if let Some(err) = parsed.error {
            return Err(anyhow!(
                "rpc error calling {method}: code={} message={} data={:?}",
                err.code,
                err.message,
                err.data
            ));
        }

        parsed
            .result
            .ok_or_else(|| anyhow!("rpc response for {method} had neither result nor error"))
    }

    pub async fn get_network(&self) -> Result<Value> {
        self.call("getNetwork", json!({})).await
    }

    /// `keys` are base64-encoded XDR `LedgerKey` values.
    pub async fn get_ledger_entries(&self, keys: Vec<String>) -> Result<Value> {
        self.call("getLedgerEntries", json!({ "keys": keys })).await
    }

    /// `envelope_xdr` is base64-encoded XDR `TransactionEnvelope`.
    pub async fn simulate_transaction(&self, envelope_xdr: &str) -> Result<Value> {
        self.call(
            "simulateTransaction",
            json!({ "transaction": envelope_xdr }),
        )
        .await
    }

    /// `envelope_xdr` is base64-encoded XDR `TransactionEnvelope` (signed).
    pub async fn send_transaction(&self, envelope_xdr: &str) -> Result<Value> {
        self.call("sendTransaction", json!({ "transaction": envelope_xdr }))
            .await
    }

    pub async fn get_transaction(&self, hash: &str) -> Result<Value> {
        self.call("getTransaction", json!({ "hash": hash })).await
    }
}

/// Small helpers for pulling typed fields out of the loosely-typed RPC JSON
/// responses, with error messages that point at what went wrong.
pub trait ValueExt {
    fn field(&self, name: &str) -> Result<&Value>;
    fn as_str_field(&self, name: &str) -> Result<&str>;
}

impl ValueExt for Value {
    fn field(&self, name: &str) -> Result<&Value> {
        self.get(name)
            .ok_or_else(|| anyhow!("expected field `{name}` in rpc response: {self}"))
    }

    fn as_str_field(&self, name: &str) -> Result<&str> {
        self.field(name)?
            .as_str()
            .ok_or_else(|| anyhow!("expected field `{name}` to be a string in: {self}"))
    }
}
