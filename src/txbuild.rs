use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::Signer;
use serde_json::Value;
use sha2::{Digest, Sha256};
use stellar_xdr::{
    AccountId, ContractId, DecoratedSignature, Hash, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, LedgerEntryData, LedgerKey, LedgerKeyAccount, Limits, Memo, Operation,
    OperationBody, Preconditions, PublicKey, ScAddress, ScSymbol, ScVal, SequenceNumber,
    Signature, SignatureHint, SorobanAuthorizationEntry, SorobanTransactionData, Transaction,
    TransactionEnvelope, TransactionExt, TransactionMeta, TransactionSignaturePayload,
    TransactionSignaturePayloadTaggedTransaction, TransactionV1Envelope, Uint256, WriteXdr,
    MuxedAccount, ReadXdr,
};

use crate::rpc::{RpcClient, ValueExt};

/// The non-resource, "inclusion" portion of the fee, on top of whatever
/// `simulateTransaction` says the Soroban resources will cost. Kept small
/// and fixed; callers priced out by network congestion can resubmit.
const INCLUSION_FEE_STROOPS: u32 = 100;

pub async fn fetch_next_sequence_number(rpc: &RpcClient, source_account: &str) -> Result<i64> {
    let pk = stellar_strkey::ed25519::PublicKey::from_string(source_account)
        .map_err(|_| anyhow!("invalid source_account `{source_account}`: expected a G... strkey"))?;
    let ledger_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(pk.0))),
    });
    let key_b64 = ledger_key.to_xdr_base64(Limits::none())?;

    let resp = rpc.get_ledger_entries(vec![key_b64]).await?;
    let entries = resp
        .field("entries")
        .ok()
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let entry = entries.first().ok_or_else(|| {
        anyhow!("source account `{source_account}` was not found on-chain (has it been funded?)")
    })?;
    let entry_xdr = entry.as_str_field("xdr")?;
    let data = LedgerEntryData::from_xdr_base64(entry_xdr, Limits::none())
        .context("failed decoding Account ledger entry")?;

    match data {
        LedgerEntryData::Account(a) => Ok(a.seq_num.0 + 1),
        other => bail!("expected an Account ledger entry, got {other:?}"),
    }
}

pub fn build_unsigned_invoke_tx(
    source_account: &str,
    next_seq: i64,
    contract_id: &str,
    function_name: &str,
    args: Vec<ScVal>,
) -> Result<TransactionEnvelope> {
    let source_pk = stellar_strkey::ed25519::PublicKey::from_string(source_account)
        .map_err(|_| anyhow!("invalid source_account `{source_account}`: expected a G... strkey"))?;
    let contract = stellar_strkey::Contract::from_string(contract_id)
        .map_err(|_| anyhow!("invalid contract_id `{contract_id}`: expected a C... strkey"))?;

    let function_name_sym: ScSymbol = function_name
        .parse()
        .map(ScSymbol)
        .map_err(|_| anyhow!("function name `{function_name}` is invalid or too long"))?;

    let invoke_args = InvokeContractArgs {
        contract_address: ScAddress::Contract(ContractId(Hash(contract.0))),
        function_name: function_name_sym,
        args: args.try_into().map_err(|_| anyhow!("too many arguments"))?,
    };

    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(invoke_args),
            auth: Default::default(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(source_pk.0)),
        fee: INCLUSION_FEE_STROOPS,
        seq_num: SequenceNumber(next_seq),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().map_err(|_| anyhow!("too many operations"))?,
        ext: TransactionExt::V0,
    };

    Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: Default::default(),
    }))
}

pub struct SimulationOutcome {
    pub soroban_data: SorobanTransactionData,
    pub min_resource_fee: i64,
    pub result_json: Option<Value>,
    pub auth_b64: Vec<String>,
    pub raw: Value,
}

pub fn parse_simulation(resp: &Value) -> Result<SimulationOutcome> {
    if let Some(err) = resp.get("error") {
        let msg = err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string());
        bail!("simulation failed: {msg}");
    }
    if resp.get("restorePreamble").is_some() {
        bail!(
            "this call requires restoring archived contract state before it can run; \
             automatic restore is not implemented by this proxy yet"
        );
    }

    let soroban_data =
        SorobanTransactionData::from_xdr_base64(resp.as_str_field("transactionData")?, Limits::none())
            .context("failed decoding simulated SorobanTransactionData")?;
    let min_resource_fee: i64 = resp
        .as_str_field("minResourceFee")?
        .parse()
        .context("minResourceFee was not a valid integer")?;

    let results = resp
        .field("results")
        .ok()
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let first = results.first();

    let result_json = first
        .and_then(|r| r.get("xdr"))
        .and_then(|v| v.as_str())
        .map(|b64| ScVal::from_xdr_base64(b64, Limits::none()))
        .transpose()
        .context("failed decoding simulated return value")?
        .map(|v| crate::scval::scval_to_json(&v));

    let auth_b64 = first
        .and_then(|r| r.get("auth"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    Ok(SimulationOutcome {
        soroban_data,
        min_resource_fee,
        result_json,
        auth_b64,
        raw: resp.clone(),
    })
}

/// Patch a freshly-built (unsigned, zero-resource) transaction with the
/// resource footprint, fee, and authorization entries produced by
/// simulation. This mirrors what `assembleTransaction` does in the JS SDK.
pub fn assemble_transaction(
    envelope: TransactionEnvelope,
    sim: &SimulationOutcome,
) -> Result<TransactionEnvelope> {
    let TransactionEnvelope::Tx(mut v1) = envelope else {
        bail!("expected a v1 transaction envelope");
    };

    let resource_fee_u32 = u32::try_from(sim.min_resource_fee)
        .map_err(|_| anyhow!("simulated resource fee out of range"))?;
    v1.tx.fee = resource_fee_u32
        .checked_add(INCLUSION_FEE_STROOPS)
        .ok_or_else(|| anyhow!("computed fee overflowed"))?;
    v1.tx.ext = TransactionExt::V1(sim.soroban_data.clone());

    if !sim.auth_b64.is_empty() {
        let auths = sim
            .auth_b64
            .iter()
            .map(|b64| {
                SorobanAuthorizationEntry::from_xdr_base64(b64, Limits::none())
                    .context("failed decoding simulated authorization entry")
            })
            .collect::<Result<Vec<_>>>()?;
        let op = v1
            .tx
            .operations
            .iter_mut()
            .next()
            .ok_or_else(|| anyhow!("built transaction unexpectedly has no operations"))?;
        if let OperationBody::InvokeHostFunction(ref mut invoke_op) = op.body {
            invoke_op.auth = auths.try_into().map_err(|_| anyhow!("too many auth entries"))?;
        }
    }

    Ok(TransactionEnvelope::Tx(v1))
}

pub fn sign_transaction(
    envelope: TransactionEnvelope,
    network_passphrase: &str,
    secret_key: &str,
) -> Result<TransactionEnvelope> {
    let TransactionEnvelope::Tx(mut v1) = envelope else {
        bail!("expected a v1 transaction envelope");
    };

    let seed = stellar_strkey::ed25519::PrivateKey::from_string(secret_key)
        .map_err(|_| anyhow!("invalid secret_key: expected an S... strkey"))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed.0);

    let network_id_bytes: [u8; 32] = Sha256::digest(network_passphrase.as_bytes()).into();
    let payload = TransactionSignaturePayload {
        network_id: Hash(network_id_bytes),
        tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(v1.tx.clone()),
    };
    let payload_xdr = payload
        .to_xdr(Limits::none())
        .context("failed encoding transaction signature payload")?;
    let tx_hash: [u8; 32] = Sha256::digest(&payload_xdr).into();

    let signature = signing_key.sign(&tx_hash);
    let verifying_key_bytes = signing_key.verifying_key().to_bytes();
    let hint = SignatureHint([
        verifying_key_bytes[28],
        verifying_key_bytes[29],
        verifying_key_bytes[30],
        verifying_key_bytes[31],
    ]);

    let decorated = DecoratedSignature {
        hint,
        signature: Signature(
            signature
                .to_bytes()
                .to_vec()
                .try_into()
                .map_err(|_| anyhow!("signature encoding failed"))?,
        ),
    };
    v1.signatures = vec![decorated]
        .try_into()
        .map_err(|_| anyhow!("too many signatures"))?;

    Ok(TransactionEnvelope::Tx(v1))
}

pub fn envelope_to_base64(envelope: &TransactionEnvelope) -> Result<String> {
    envelope
        .to_xdr_base64(Limits::none())
        .context("failed encoding transaction envelope")
}

/// Pull the contract's return value out of a `getTransaction` result's
/// `resultMetaXdr`. Recent RPC versions no longer surface a top-level
/// `returnValue` field, so this must be dug out of the Soroban meta instead.
pub fn extract_return_value(result_meta_xdr_b64: &str) -> Result<Option<ScVal>> {
    let meta = TransactionMeta::from_xdr_base64(result_meta_xdr_b64, Limits::none())
        .context("failed decoding resultMetaXdr")?;
    Ok(match meta {
        TransactionMeta::V3(v3) => v3.soroban_meta.map(|m| m.return_value),
        TransactionMeta::V4(v4) => v4.soroban_meta.and_then(|m| m.return_value),
        _ => None,
    })
}
