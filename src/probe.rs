//! At startup, classify each contract function as `GET`-eligible or
//! `POST`-only:
//!
//! - Functions that take any arguments are always `POST` — arguments belong
//!   in a body, not scattered across query parameters, regardless of
//!   whether the call happens to be read-only.
//! - Argument-less functions are simulated once with a synthetic, unfunded
//!   source account; if the resulting footprint has no `read_write`
//!   entries, the function is read-only and safe to expose as `GET`.
//!
//! This never touches real chain state: `simulateTransaction` runs against
//! an ephemeral sandbox and nothing here is signed or submitted.

use std::collections::HashMap;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::rpc::RpcClient;
use crate::spec::ContractSpec;
use crate::txbuild;

/// Sequence number used to build probe-only transactions. Never submitted,
/// so it doesn't need to reflect a real account's actual sequence.
const PROBE_SEQ_NUM: i64 = 1;

/// A syntactically valid G... address that (almost certainly) doesn't
/// correspond to any real funded account. Derived deterministically so
/// startup logs are stable across restarts; never signed with.
fn probe_source_account() -> String {
    let bytes: [u8; 32] = Sha256::digest(b"soroban-api-proxy:startup-probe-account").into();
    format!("{}", stellar_strkey::ed25519::PublicKey(bytes))
}

/// Simulate every argument-less function once and return whether each
/// function in `spec` is `GET`-eligible (`true`) or `POST`-only (`false`),
/// keyed by function name. Functions that take arguments are `POST`-only
/// without needing simulation. Functions that fail to simulate (e.g. they
/// trap even with no arguments) are conservatively classified as `POST`,
/// since that's the safer default for an API surface a caller might poke at
/// without checking logs first.
pub async fn classify_functions(
    rpc: &RpcClient,
    contract_id: &str,
    spec: &ContractSpec,
) -> HashMap<String, bool> {
    let probe_account = probe_source_account();
    let mut methods = HashMap::with_capacity(spec.functions.len());

    for f in &spec.functions {
        let name = f.name.0.to_utf8_string_lossy();

        if !f.inputs.is_empty() {
            methods.insert(name, false);
            continue;
        }

        match probe_no_arg_function(rpc, contract_id, &probe_account, &name).await {
            Ok(read_only) => {
                methods.insert(name, read_only);
            }
            Err(e) => {
                tracing::warn!(
                    function = %name,
                    error = %e,
                    "could not classify function at startup; defaulting to POST"
                );
                methods.insert(name, false);
            }
        }
    }

    methods
}

async fn probe_no_arg_function(
    rpc: &RpcClient,
    contract_id: &str,
    probe_account: &str,
    name: &str,
) -> Result<bool> {
    let unsigned =
        txbuild::build_unsigned_invoke_tx(probe_account, PROBE_SEQ_NUM, contract_id, name, Vec::new())?;
    let unsigned_b64 = txbuild::envelope_to_base64(&unsigned)?;
    let sim_resp = rpc.simulate_transaction(&unsigned_b64).await?;
    let sim = txbuild::parse_simulation(&sim_resp)?;

    Ok(sim.soroban_data.resources.footprint.read_write.is_empty())
}
