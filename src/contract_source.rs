use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use stellar_xdr::{
    Hash, LedgerEntryData, LedgerKey, LedgerKeyContractCode, Limits, ReadXdr, WriteXdr,
};

use crate::rpc::{RpcClient, ValueExt};
use crate::spec::{read_spec_entries_from_wasm, ContractSpec};

pub fn parse_wasm_hash(wasm_hash_hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(wasm_hash_hex.trim())
        .map_err(|e| anyhow!("wasm hash must be hex-encoded: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("wasm hash must be exactly 32 bytes (64 hex characters)"))
}

/// Fetch a contract's deployed WASM bytecode from the network by its hash.
pub async fn fetch_wasm_by_hash(rpc: &RpcClient, wasm_hash_hex: &str) -> Result<Vec<u8>> {
    let hash_bytes = parse_wasm_hash(wasm_hash_hex)?;
    let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash(hash_bytes),
    });
    let key_b64 = ledger_key
        .to_xdr_base64(Limits::none())
        .context("failed encoding ledger key")?;

    let resp = rpc.get_ledger_entries(vec![key_b64]).await?;
    let entries = resp
        .field("entries")
        .ok()
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let entry = entries.first().ok_or_else(|| {
        anyhow!(
            "no contract WASM found on-chain for hash `{wasm_hash_hex}` \
             (it may not be uploaded on this network, or the hash is wrong)"
        )
    })?;
    let entry_xdr = entry.as_str_field("xdr")?;
    let data = LedgerEntryData::from_xdr_base64(entry_xdr, Limits::none())
        .context("failed decoding ContractCode ledger entry")?;

    match data {
        LedgerEntryData::ContractCode(c) => Ok(c.code.to_vec()),
        other => bail!("expected a ContractCode ledger entry, got {other:?}"),
    }
}

/// Fetch (or return from cache) the parsed contract spec for a given WASM
/// hash. This is the primary entry point used by the HTTP layer.
pub async fn get_or_fetch_spec(
    cache: &tokio::sync::RwLock<std::collections::HashMap<String, Arc<ContractSpec>>>,
    rpc: &RpcClient,
    wasm_hash_hex: &str,
) -> Result<Arc<ContractSpec>> {
    let key = wasm_hash_hex.trim().to_lowercase();
    // Validate shape early so bad input doesn't get cached under a bogus key.
    parse_wasm_hash(&key)?;

    if let Some(spec) = cache.read().await.get(&key) {
        return Ok(spec.clone());
    }

    let wasm = fetch_wasm_by_hash(rpc, &key).await?;
    let entries = read_spec_entries_from_wasm(&wasm)?;
    let spec = Arc::new(ContractSpec::from_entries(entries));

    cache.write().await.insert(key, spec.clone());
    Ok(spec)
}
