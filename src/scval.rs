//! Type-directed conversion between plain JSON (what a REST caller sends)
//! and Soroban's `ScVal` (what the contract actually expects), driven by the
//! function's parsed contract spec.
//!
//! For anything not covered by the friendly conversion below (u256/i256,
//! exotic address kinds, raw opaque values, ...) callers can always supply
//! `{"__xdr": <value>}` for any single argument, where `<value>` is the
//! serde-JSON form of the `ScVal` itself. That bypasses type-directed
//! conversion entirely and is decoded as-is.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value as J};
use stellar_xdr::{
    AccountId, ContractId, Duration, Hash, Int128Parts, LedgerKey, PublicKey, ScAddress, ScBytes,
    ScMap, ScMapEntry, ScSpecTypeDef, ScSpecUdtEnumV0, ScSpecUdtErrorEnumV0, ScSpecUdtStructV0,
    ScSpecUdtUnionCaseV0, ScString, ScSymbol, ScVal, ScVec, StringM, TimePoint, UInt128Parts,
    Uint256,
};

use crate::spec::ContractSpec;

pub fn json_to_scval(value: &J, ty: &ScSpecTypeDef, spec: &ContractSpec) -> Result<ScVal> {
    if let J::Object(map) = value
        && let Some(x) = map.get("__xdr") {
            return serde_json::from_value(x.clone())
                .map_err(|e| anyhow!("invalid `__xdr` escape-hatch value: {e}"));
        }

    match ty {
        ScSpecTypeDef::Val => bail!("`val` args must be supplied via {{\"__xdr\": ...}}"),
        ScSpecTypeDef::Bool => Ok(ScVal::Bool(
            value.as_bool().ok_or_else(|| anyhow!("expected a boolean"))?,
        )),
        ScSpecTypeDef::Void => Ok(ScVal::Void),
        ScSpecTypeDef::Error => bail!("`error` values cannot be supplied as arguments"),
        ScSpecTypeDef::U32 => Ok(ScVal::U32(
            u32::try_from(as_u64(value)?).map_err(|_| anyhow!("u32 value out of range"))?,
        )),
        ScSpecTypeDef::I32 => Ok(ScVal::I32(
            i32::try_from(as_i64(value)?).map_err(|_| anyhow!("i32 value out of range"))?,
        )),
        ScSpecTypeDef::U64 => Ok(ScVal::U64(as_u64(value)?)),
        ScSpecTypeDef::I64 => Ok(ScVal::I64(as_i64(value)?)),
        ScSpecTypeDef::Timepoint => Ok(ScVal::Timepoint(TimePoint(as_u64(value)?))),
        ScSpecTypeDef::Duration => Ok(ScVal::Duration(Duration(as_u64(value)?))),
        ScSpecTypeDef::U128 => Ok(ScVal::U128(u128_to_parts(as_u128(value)?))),
        ScSpecTypeDef::I128 => Ok(ScVal::I128(i128_to_parts(as_i128(value)?))),
        ScSpecTypeDef::U256 => bail!("u256 args must be supplied via {{\"__xdr\": ...}}"),
        ScSpecTypeDef::I256 => bail!("i256 args must be supplied via {{\"__xdr\": ...}}"),
        ScSpecTypeDef::Bytes => Ok(ScVal::Bytes(ScBytes(
            bytes_from_json(value)?
                .try_into()
                .map_err(|_| anyhow!("bytes value too long"))?,
        ))),
        ScSpecTypeDef::String => Ok(ScVal::String(ScString(
            value
                .as_str()
                .ok_or_else(|| anyhow!("expected a string"))?
                .parse()
                .map_err(|_| anyhow!("string value too long"))?,
        ))),
        ScSpecTypeDef::Symbol => Ok(ScVal::Symbol(symbol_from_str(
            value.as_str().ok_or_else(|| anyhow!("expected a string for symbol"))?,
        )?)),
        ScSpecTypeDef::Address | ScSpecTypeDef::MuxedAddress => Ok(ScVal::Address(
            address_from_str(value.as_str().ok_or_else(|| anyhow!("expected an address string"))?)?,
        )),
        ScSpecTypeDef::Option(o) => {
            if value.is_null() {
                Ok(ScVal::Void)
            } else {
                json_to_scval(value, &o.value_type, spec)
            }
        }
        ScSpecTypeDef::Result(_) => {
            bail!("result-typed args must be supplied via {{\"__xdr\": ...}}")
        }
        ScSpecTypeDef::Vec(v) => {
            let arr = value.as_array().ok_or_else(|| anyhow!("expected an array"))?;
            let items = arr
                .iter()
                .map(|x| json_to_scval(x, &v.element_type, spec))
                .collect::<Result<Vec<_>>>()?;
            Ok(ScVal::Vec(Some(ScVec(
                items.try_into().map_err(|_| anyhow!("vec value too long"))?,
            ))))
        }
        ScSpecTypeDef::Map(m) => {
            let obj = value
                .as_object()
                .ok_or_else(|| anyhow!("expected an object for a map argument"))?;
            let mut entries = Vec::with_capacity(obj.len());
            for (k, v) in obj {
                let key = json_key_to_scval(k, &m.key_type)?;
                let val = json_to_scval(v, &m.value_type, spec)?;
                entries.push(ScMapEntry { key, val });
            }
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(ScVal::Map(Some(ScMap(
                entries.try_into().map_err(|_| anyhow!("map value too large"))?,
            ))))
        }
        ScSpecTypeDef::Tuple(t) => {
            let arr = value
                .as_array()
                .ok_or_else(|| anyhow!("expected an array for a tuple argument"))?;
            if arr.len() != t.value_types.len() {
                bail!(
                    "tuple arity mismatch: expected {} element(s), got {}",
                    t.value_types.len(),
                    arr.len()
                );
            }
            let items = arr
                .iter()
                .zip(t.value_types.iter())
                .map(|(v, ty)| json_to_scval(v, ty, spec))
                .collect::<Result<Vec<_>>>()?;
            Ok(ScVal::Vec(Some(ScVec(
                items.try_into().map_err(|_| anyhow!("tuple value too long"))?,
            ))))
        }
        ScSpecTypeDef::BytesN(n) => {
            let bytes = bytes_from_json(value)?;
            if bytes.len() != n.n as usize {
                bail!("expected exactly {} byte(s), got {}", n.n, bytes.len());
            }
            Ok(ScVal::Bytes(ScBytes(
                bytes.try_into().map_err(|_| anyhow!("bytesN conversion failed"))?,
            )))
        }
        ScSpecTypeDef::Udt(u) => udt_to_scval(&u.name.to_utf8_string_lossy(), value, spec),
    }
}

fn udt_to_scval(name: &str, value: &J, spec: &ContractSpec) -> Result<ScVal> {
    if let Some(s) = spec.structs.get(name) {
        return struct_to_scval(s, value, spec);
    }
    if spec.unions.contains_key(name) {
        return union_to_scval(name, value, spec);
    }
    if let Some(e) = spec.enums.get(name) {
        return enum_to_scval(e, value);
    }
    if let Some(e) = spec.error_enums.get(name) {
        return error_enum_to_scval(e, value);
    }
    bail!("contract spec does not define user-defined type `{name}`")
}

fn struct_to_scval(s: &ScSpecUdtStructV0, value: &J, spec: &ContractSpec) -> Result<ScVal> {
    let name = s.name.to_utf8_string_lossy();
    let is_tuple_struct = s
        .fields
        .iter()
        .enumerate()
        .all(|(i, f)| f.name.to_utf8_string_lossy() == i.to_string());

    if is_tuple_struct && !s.fields.is_empty() {
        let arr = value
            .as_array()
            .ok_or_else(|| anyhow!("expected an array for tuple struct `{name}`"))?;
        if arr.len() != s.fields.len() {
            bail!(
                "struct `{name}` expects {} field(s), got {}",
                s.fields.len(),
                arr.len()
            );
        }
        let items = arr
            .iter()
            .zip(s.fields.iter())
            .map(|(v, f)| json_to_scval(v, &f.type_, spec))
            .collect::<Result<Vec<_>>>()?;
        Ok(ScVal::Vec(Some(ScVec(
            items.try_into().map_err(|_| anyhow!("struct `{name}` has too many fields"))?,
        ))))
    } else {
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("expected an object for struct `{name}`"))?;
        let mut entries = Vec::with_capacity(s.fields.len());
        for f in s.fields.iter() {
            let fname = f.name.to_utf8_string_lossy();
            let fv = obj
                .get(&fname)
                .ok_or_else(|| anyhow!("struct `{name}` is missing field `{fname}`"))?;
            let key = ScVal::Symbol(symbol_from_str(&fname)?);
            let val = json_to_scval(fv, &f.type_, spec)?;
            entries.push(ScMapEntry { key, val });
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(ScVal::Map(Some(ScMap(
            entries.try_into().map_err(|_| anyhow!("struct `{name}` has too many fields"))?,
        ))))
    }
}

fn union_to_scval(union_name: &str, value: &J, spec: &ContractSpec) -> Result<ScVal> {
    let (case_name, args_json): (String, Vec<J>) = match value {
        J::String(s) => (s.clone(), vec![]),
        J::Object(m) if m.len() == 1 => {
            let (k, v) = m.iter().next().expect("checked len == 1");
            let args = match v {
                J::Array(a) => a.clone(),
                J::Null => vec![],
                other => vec![other.clone()],
            };
            (k.clone(), args)
        }
        _ => bail!(
            "expected a case name string, or a single-key object of {{\"CaseName\": [args...]}}, \
             for union `{union_name}`"
        ),
    };

    let case = spec
        .union_case(union_name, &case_name)
        .ok_or_else(|| anyhow!("union `{union_name}` has no case `{case_name}`"))?;

    let mut items = vec![ScVal::Symbol(symbol_from_str(&case_name)?)];
    match case {
        ScSpecUdtUnionCaseV0::VoidV0(_) => {
            if !args_json.is_empty() {
                bail!("case `{case_name}` of union `{union_name}` takes no arguments");
            }
        }
        ScSpecUdtUnionCaseV0::TupleV0(t) => {
            if args_json.len() != t.type_.len() {
                bail!(
                    "case `{case_name}` of union `{union_name}` expects {} argument(s), got {}",
                    t.type_.len(),
                    args_json.len()
                );
            }
            for (v, ty) in args_json.iter().zip(t.type_.iter()) {
                items.push(json_to_scval(v, ty, spec)?);
            }
        }
    }
    Ok(ScVal::Vec(Some(ScVec(
        items.try_into().map_err(|_| anyhow!("union value too large"))?,
    ))))
}

fn enum_to_scval(e: &ScSpecUdtEnumV0, value: &J) -> Result<ScVal> {
    let name = e.name.to_utf8_string_lossy();
    if let Some(n) = value.as_u64() {
        let n = u32::try_from(n).map_err(|_| anyhow!("enum value out of range"))?;
        if !e.cases.iter().any(|c| c.value == n) {
            bail!("enum `{name}` has no case with value {n}");
        }
        return Ok(ScVal::U32(n));
    }
    if let Some(s) = value.as_str() {
        let case = e
            .cases
            .iter()
            .find(|c| c.name.to_utf8_string_lossy() == s)
            .ok_or_else(|| anyhow!("enum `{name}` has no case `{s}`"))?;
        return Ok(ScVal::U32(case.value));
    }
    bail!("expected a case name string or integer for enum `{name}`")
}

fn error_enum_to_scval(e: &ScSpecUdtErrorEnumV0, value: &J) -> Result<ScVal> {
    let name = e.name.to_utf8_string_lossy();
    if let Some(n) = value.as_u64() {
        let n = u32::try_from(n).map_err(|_| anyhow!("error enum value out of range"))?;
        return Ok(ScVal::U32(n));
    }
    if let Some(s) = value.as_str() {
        let case = e
            .cases
            .iter()
            .find(|c| c.name.to_utf8_string_lossy() == s)
            .ok_or_else(|| anyhow!("error enum `{name}` has no case `{s}`"))?;
        return Ok(ScVal::U32(case.value));
    }
    bail!("expected a case name string or integer for error enum `{name}`")
}

fn json_key_to_scval(key: &str, key_type: &ScSpecTypeDef) -> Result<ScVal> {
    match key_type {
        ScSpecTypeDef::Symbol => Ok(ScVal::Symbol(symbol_from_str(key)?)),
        ScSpecTypeDef::String => Ok(ScVal::String(ScString(
            key.parse().map_err(|_| anyhow!("map key string too long"))?,
        ))),
        ScSpecTypeDef::Address | ScSpecTypeDef::MuxedAddress => {
            Ok(ScVal::Address(address_from_str(key)?))
        }
        ScSpecTypeDef::Bool => Ok(ScVal::Bool(
            key.parse().map_err(|_| anyhow!("invalid boolean map key `{key}`"))?,
        )),
        ScSpecTypeDef::U32 => Ok(ScVal::U32(
            key.parse().map_err(|_| anyhow!("invalid u32 map key `{key}`"))?,
        )),
        ScSpecTypeDef::I32 => Ok(ScVal::I32(
            key.parse().map_err(|_| anyhow!("invalid i32 map key `{key}`"))?,
        )),
        ScSpecTypeDef::U64 => Ok(ScVal::U64(
            key.parse().map_err(|_| anyhow!("invalid u64 map key `{key}`"))?,
        )),
        ScSpecTypeDef::I64 => Ok(ScVal::I64(
            key.parse().map_err(|_| anyhow!("invalid i64 map key `{key}`"))?,
        )),
        other => bail!(
            "map keys of type {:?} must be supplied via a whole-argument {{\"__xdr\": ...}} map",
            other
        ),
    }
}

fn symbol_from_str(s: &str) -> Result<ScSymbol> {
    let m: StringM<32> = s.parse().map_err(|_| anyhow!("symbol `{s}` is invalid or too long"))?;
    Ok(ScSymbol(m))
}

fn bytes_from_json(value: &J) -> Result<Vec<u8>> {
    match value {
        J::String(s) => {
            let s = s.strip_prefix("0x").unwrap_or(s);
            hex::decode(s).map_err(|e| anyhow!("invalid hex bytes: {e}"))
        }
        J::Array(a) => a
            .iter()
            .map(|x| {
                x.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| anyhow!("byte array elements must be integers 0-255"))
            })
            .collect(),
        _ => bail!("expected a hex string (optionally 0x-prefixed) or an array of byte values"),
    }
}

fn as_u64(v: &J) -> Result<u64> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().map_err(|e| anyhow!("invalid integer `{s}`: {e}"));
    }
    bail!("expected an integer or a numeric string")
}

fn as_i64(v: &J) -> Result<i64> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().map_err(|e| anyhow!("invalid integer `{s}`: {e}"));
    }
    bail!("expected an integer or a numeric string")
}

fn as_u128(v: &J) -> Result<u128> {
    if let Some(n) = v.as_u64() {
        return Ok(n as u128);
    }
    if let Some(s) = v.as_str() {
        return s.parse().map_err(|e| anyhow!("invalid u128 `{s}`: {e}"));
    }
    bail!("expected an integer or a numeric string")
}

fn as_i128(v: &J) -> Result<i128> {
    if let Some(n) = v.as_i64() {
        return Ok(n as i128);
    }
    if let Some(s) = v.as_str() {
        return s.parse().map_err(|e| anyhow!("invalid i128 `{s}`: {e}"));
    }
    bail!("expected an integer or a numeric string")
}

fn u128_to_parts(v: u128) -> UInt128Parts {
    UInt128Parts {
        hi: (v >> 64) as u64,
        lo: v as u64,
    }
}

fn i128_to_parts(v: i128) -> Int128Parts {
    Int128Parts {
        hi: (v >> 64) as i64,
        lo: (v as u128 & 0xFFFF_FFFF_FFFF_FFFF) as u64,
    }
}

fn u128_from_parts(p: &UInt128Parts) -> u128 {
    ((p.hi as u128) << 64) | (p.lo as u128)
}

fn i128_from_parts(p: &Int128Parts) -> i128 {
    ((p.hi as i128) << 64) | (p.lo as i128)
}

pub fn address_from_str(s: &str) -> Result<ScAddress> {
    if let Ok(pk) = stellar_strkey::ed25519::PublicKey::from_string(s) {
        return Ok(ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(
            Uint256(pk.0),
        ))));
    }
    if let Ok(c) = stellar_strkey::Contract::from_string(s) {
        return Ok(ScAddress::Contract(ContractId(Hash(c.0))));
    }
    bail!("invalid address `{s}`: expected a G... account or C... contract strkey")
}

pub fn address_to_string(a: &ScAddress) -> String {
    match a {
        ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))) => {
            format!("{}", stellar_strkey::ed25519::PublicKey(*bytes))
        }
        ScAddress::Contract(ContractId(Hash(bytes))) => {
            format!("{}", stellar_strkey::Contract(*bytes))
        }
        other => format!("{other:?}"),
    }
}

fn account_id_to_string(id: &AccountId) -> String {
    let AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))) = id;
    format!("{}", stellar_strkey::ed25519::PublicKey(*bytes))
}

/// Readable rendering of a `LedgerKey`, for surfacing a simulated call's
/// read-write (or read-only) footprint to API callers.
pub fn ledger_key_to_json(key: &LedgerKey) -> J {
    match key {
        LedgerKey::Account(a) => json!({
            "type": "account",
            "account_id": account_id_to_string(&a.account_id),
        }),
        LedgerKey::ContractData(cd) => {
            let key = if matches!(cd.key, ScVal::LedgerKeyContractInstance) {
                J::String("instance".to_string())
            } else {
                scval_to_json(&cd.key)
            };
            json!({
                "type": "contract_data",
                "contract": address_to_string(&cd.contract),
                "key": key,
                "durability": format!("{:?}", cd.durability),
            })
        }
        LedgerKey::ContractCode(cc) => json!({
            "type": "contract_code",
            "hash": hex::encode(cc.hash.0),
        }),
        other => json!({
            "type": "other",
            "debug": format!("{other:?}"),
        }),
    }
}

/// Generic, spec-independent rendering of a return value as JSON. 64-bit and
/// wider integers are rendered as decimal strings to avoid precision loss in
/// JSON consumers.
pub fn scval_to_json(v: &ScVal) -> J {
    match v {
        ScVal::Bool(b) => J::Bool(*b),
        ScVal::Void => J::Null,
        ScVal::Error(e) => json!({ "error": format!("{e:?}") }),
        ScVal::U32(n) => json!(n),
        ScVal::I32(n) => json!(n),
        ScVal::U64(n) => J::String(n.to_string()),
        ScVal::I64(n) => J::String(n.to_string()),
        ScVal::Timepoint(t) => J::String(t.0.to_string()),
        ScVal::Duration(d) => J::String(d.0.to_string()),
        ScVal::U128(p) => J::String(u128_from_parts(p).to_string()),
        ScVal::I128(p) => J::String(i128_from_parts(p).to_string()),
        ScVal::U256(_) | ScVal::I256(_) => json!({ "note": "256-bit value; refetch with raw XDR for full precision" }),
        ScVal::Bytes(b) => J::String(format!("0x{}", hex::encode(b.0.as_slice()))),
        ScVal::String(s) => J::String(s.0.to_utf8_string_lossy()),
        ScVal::Symbol(s) => J::String(s.0.to_utf8_string_lossy()),
        ScVal::Vec(Some(vec)) => J::Array(vec.0.iter().map(scval_to_json).collect()),
        ScVal::Vec(None) => J::Array(vec![]),
        ScVal::Map(Some(map)) => {
            let all_string_keys = map
                .0
                .iter()
                .all(|e| matches!(e.key, ScVal::Symbol(_) | ScVal::String(_)));
            if all_string_keys {
                let mut obj = serde_json::Map::new();
                for entry in map.0.iter() {
                    let k = match &entry.key {
                        ScVal::Symbol(s) => s.0.to_utf8_string_lossy(),
                        ScVal::String(s) => s.0.to_utf8_string_lossy(),
                        _ => unreachable!("checked above"),
                    };
                    obj.insert(k, scval_to_json(&entry.val));
                }
                J::Object(obj)
            } else {
                J::Array(
                    map.0
                        .iter()
                        .map(|e| json!([scval_to_json(&e.key), scval_to_json(&e.val)]))
                        .collect(),
                )
            }
        }
        ScVal::Map(None) => J::Object(Default::default()),
        ScVal::Address(a) => J::String(address_to_string(a)),
        ScVal::ContractInstance(_) => json!({ "note": "contract instance value" }),
        ScVal::LedgerKeyContractInstance => J::Null,
        ScVal::LedgerKeyNonce(n) => json!({ "nonce": n.nonce.to_string() }),
    }
}
