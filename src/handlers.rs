use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use stellar_xdr::{Limits, ReadXdr, ScVal};

use crate::contract_source::get_or_fetch_spec;
use crate::error::{AppError, AppResult};
use crate::scval;
use crate::state::AppState;
use crate::txbuild;

/// Best-effort classification of a spec/wasm lookup failure into an HTTP
/// status. Lookup failures are almost always caller mistakes (bad hash,
/// unknown contract) or a flaky upstream RPC; distinguish on message content.
fn classify_lookup_err(e: anyhow::Error) -> AppError {
    let msg = e.to_string();
    if msg.contains("must be exactly 32 bytes") || msg.contains("must be hex-encoded") {
        AppError::bad_request(msg)
    } else if msg.contains("no contract WASM found") {
        AppError::not_found(msg)
    } else if msg.contains("no `contractspecv0`") {
        AppError::bad_request(msg)
    } else {
        AppError::upstream(msg)
    }
}

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn get_network(State(state): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let network = state
        .rpc
        .get_network()
        .await
        .map_err(|e| AppError::upstream(e.to_string()))?;
    Ok(Json(network))
}

pub async fn get_spec(State(state): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let spec = get_or_fetch_spec(&state.spec_cache, &state.rpc, &state.contract_wasm_hash)
        .await
        .map_err(classify_lookup_err)?;
    Ok(Json(spec.to_json()))
}

pub async fn get_functions(State(state): State<Arc<AppState>>) -> AppResult<Json<Value>> {
    let spec = get_or_fetch_spec(&state.spec_cache, &state.rpc, &state.contract_wasm_hash)
        .await
        .map_err(classify_lookup_err)?;
    let methods = state.function_methods.read().await;

    let functions: Vec<Value> = spec
        .functions
        .iter()
        .map(|f| {
            let mut v = crate::spec::function_to_json(f);
            let name = f.name.0.to_utf8_string_lossy();
            let http_method = if methods.get(&name).copied().unwrap_or(false) {
                "GET"
            } else {
                "POST"
            };
            v.as_object_mut()
                .expect("function_to_json always returns an object")
                .insert("http_method".to_string(), json!(http_method));
            v
        })
        .collect();
    Ok(Json(json!(functions)))
}

const HEADER_SOURCE_ACCOUNT: &str = "x-source-account";
const HEADER_SIGN: &str = "x-sign";
const HEADER_SECRET_KEY: &str = "x-secret-key";

/// Read a header as a UTF-8 string, if present.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> AppResult<Option<&'a str>> {
    match headers.get(name) {
        Some(v) => v
            .to_str()
            .map(Some)
            .map_err(|_| AppError::bad_request(format!("header `{name}` is not valid UTF-8"))),
        None => Ok(None),
    }
}

/// Parse a raw query-string value as JSON where possible (so `5`, `true`,
/// `[1,2]`, `{"x":1}` all come through as their natural JSON type), falling
/// back to a plain JSON string for anything that isn't valid JSON on its own
/// (e.g. a bare word, or a G.../C... address).
fn query_value_to_json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Read the required source account plus the optional sign/secret-key
/// headers, shared by both the `GET` and `POST` invoke handlers.
fn read_common_headers(headers: &HeaderMap) -> AppResult<(String, bool, Option<String>)> {
    let source_account = header_str(headers, HEADER_SOURCE_ACCOUNT)?
        .ok_or_else(|| AppError::bad_request(format!("missing required header `{HEADER_SOURCE_ACCOUNT}`")))?
        .to_string();
    let sign = header_str(headers, HEADER_SIGN)?
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);
    let secret_key = header_str(headers, HEADER_SECRET_KEY)?.map(str::to_string);
    Ok((source_account, sign, secret_key))
}

/// `GET /api/{function_name}` — argument-less functions only. Arguments
/// come from query parameters (each parsed as JSON where possible); signing
/// works the same as on `POST` via `X-Sign`/`X-Secret-Key`.
pub async fn invoke_get(
    State(state): State<Arc<AppState>>,
    Path(function_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> AppResult<Json<Value>> {
    let (source_account, sign, secret_key) = read_common_headers(&headers)?;
    let args: serde_json::Map<String, Value> = query
        .into_iter()
        .map(|(k, v)| (k, query_value_to_json(&v)))
        .collect();

    invoke_inner(&state, &function_name, args, &source_account, sign, secret_key, true).await
}

/// `POST /api/{function_name}` — everything else. Arguments come from
/// the JSON body; sign/submit is opt-in via headers.
pub async fn invoke_post(
    State(state): State<Arc<AppState>>,
    Path(function_name): Path<String>,
    headers: HeaderMap,
    Json(args): Json<serde_json::Map<String, Value>>,
) -> AppResult<Json<Value>> {
    let (source_account, sign, secret_key) = read_common_headers(&headers)?;
    invoke_inner(&state, &function_name, args, &source_account, sign, secret_key, false).await
}

#[allow(clippy::too_many_arguments)]
async fn invoke_inner(
    state: &AppState,
    function_name: &str,
    args: serde_json::Map<String, Value>,
    source_account: &str,
    sign: bool,
    secret_key: Option<String>,
    is_get: bool,
) -> AppResult<Json<Value>> {
    let spec = get_or_fetch_spec(&state.spec_cache, &state.rpc, &state.contract_wasm_hash)
        .await
        .map_err(classify_lookup_err)?;

    let func_spec = spec
        .function(function_name)
        .ok_or_else(|| AppError::not_found(format!("contract has no function `{function_name}`")))?;

    let use_get = state.should_use_get(function_name).await;
    if is_get && !use_get {
        let reason = if !func_spec.inputs.is_empty() {
            "it takes arguments"
        } else {
            "it changes contract state"
        };
        return Err(AppError::method_not_allowed(format!(
            "function `{function_name}` must be called with POST ({reason}); use POST instead of GET"
        )));
    }
    if !is_get && use_get {
        return Err(AppError::method_not_allowed(format!(
            "function `{function_name}` takes no arguments and is read-only; \
             call it with GET instead of POST"
        )));
    }

    let mut scvals = Vec::with_capacity(func_spec.inputs.len());
    for input in func_spec.inputs.iter() {
        let pname = input.name.to_utf8_string_lossy();
        let jv = args.get(&pname).cloned().unwrap_or(Value::Null);
        let sv = scval::json_to_scval(&jv, &input.type_, &spec)
            .map_err(|e| AppError::bad_request(format!("argument `{pname}`: {e}")))?;
        scvals.push(sv);
    }

    let next_seq = txbuild::fetch_next_sequence_number(&state.rpc, source_account)
        .await
        .map_err(|e| AppError::bad_request(e.to_string()))?;

    let unsigned = txbuild::build_unsigned_invoke_tx(
        source_account,
        next_seq,
        &state.contract_id,
        function_name,
        scvals,
    )
    .map_err(|e| AppError::bad_request(e.to_string()))?;
    let unsigned_b64 = txbuild::envelope_to_base64(&unsigned)?;

    let sim_resp = state
        .rpc
        .simulate_transaction(&unsigned_b64)
        .await
        .map_err(|e| AppError::upstream(e.to_string()))?;
    let sim = txbuild::parse_simulation(&sim_resp).map_err(|e| AppError::bad_request(e.to_string()))?;
    let assembled = txbuild::assemble_transaction(unsigned, &sim)?;

    if !sign {
        let transaction_xdr = txbuild::envelope_to_base64(&assembled)?;
        let read_write: Vec<Value> = sim
            .soroban_data
            .resources
            .footprint
            .read_write
            .iter()
            .map(scval::ledger_key_to_json)
            .collect();
        let note = format!(
            "Sign this XDR yourself and submit it (e.g. via sendTransaction), \
             or resend this request with a `{HEADER_SIGN}: true` header and a \
             `{HEADER_SECRET_KEY}` header to have the proxy sign and submit it on your behalf."
        );
        return Ok(Json(json!({
            "status": "simulated",
            "network_passphrase": state.network_passphrase,
            "simulated_result": sim.result_json,
            "min_resource_fee": sim.min_resource_fee.to_string(),
            "read_write": read_write,
            "transaction_xdr": transaction_xdr,
            "note": note,
            "simulation": sim.raw,
        })));
    }

    let secret_key = secret_key.ok_or_else(|| {
        AppError::bad_request(format!(
            "header `{HEADER_SECRET_KEY}` is required when `{HEADER_SIGN}: true`"
        ))
    })?;
    let signed = txbuild::sign_transaction(assembled, &state.network_passphrase, &secret_key)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let signed_b64 = txbuild::envelope_to_base64(&signed)?;

    let send_resp = state
        .rpc
        .send_transaction(&signed_b64)
        .await
        .map_err(|e| AppError::upstream(e.to_string()))?;
    let send_status = send_resp.get("status").and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
    if send_status == "ERROR" {
        return Err(AppError::bad_request(format!(
            "network rejected the transaction: {send_resp}"
        )));
    }
    let hash = send_resp
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::upstream("sendTransaction response had no hash"))?
        .to_string();

    let mut final_status: Option<Value> = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let get_resp = state
            .rpc
            .get_transaction(&hash)
            .await
            .map_err(|e| AppError::upstream(e.to_string()))?;
        let st = get_resp.get("status").and_then(|v| v.as_str()).unwrap_or("NOT_FOUND");
        if st == "NOT_FOUND" {
            continue;
        }
        final_status = Some(get_resp);
        break;
    }

    let get_resp = final_status.ok_or_else(|| {
        AppError::upstream(format!(
            "timed out waiting for transaction {hash} to be included in a ledger; \
             it may still confirm later, check getTransaction manually"
        ))
    })?;

    let status = get_resp
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let return_value = get_resp
        .get("returnValue")
        .and_then(|v| v.as_str())
        .and_then(|b64| ScVal::from_xdr_base64(b64, Limits::none()).ok())
        .or_else(|| {
            get_resp
                .get("resultMetaXdr")
                .and_then(|v| v.as_str())
                .and_then(|b64| txbuild::extract_return_value(b64).ok().flatten())
        })
        .map(|v| scval::scval_to_json(&v));

    Ok(Json(json!({
        "status": status,
        "hash": hash,
        "return_value": return_value,
        "raw": get_resp,
    })))
}
