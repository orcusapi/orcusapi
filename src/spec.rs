use std::collections::HashMap;
use std::io::Cursor;

use anyhow::{anyhow, Context, Result};
use stellar_xdr::{
    Limited, Limits, ReadXdr, ScSpecEntry, ScSpecFunctionV0, ScSpecTypeDef, ScSpecUdtEnumV0,
    ScSpecUdtErrorEnumV0, ScSpecUdtStructV0, ScSpecUdtUnionCaseV0, ScSpecUdtUnionV0,
};

const CONTRACT_SPEC_SECTION: &str = "contractspecv0";

/// Extract the raw stream of `ScSpecEntry` XDR values embedded in a Soroban
/// contract's WASM custom section (`contractspecv0`).
pub fn read_spec_entries_from_wasm(wasm: &[u8]) -> Result<Vec<ScSpecEntry>> {
    let mut section_data: Option<&[u8]> = None;

    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload.context("failed parsing WASM module")?;
        if let wasmparser::Payload::CustomSection(reader) = payload
            && reader.name() == CONTRACT_SPEC_SECTION {
                section_data = Some(reader.data());
                break;
            }
    }

    let data = section_data.ok_or_else(|| {
        anyhow!(
            "WASM module has no `{CONTRACT_SPEC_SECTION}` custom section; \
             it may not be a Soroban contract, or was built without contract metadata"
        )
    })?;

    let cursor = Cursor::new(data);
    let mut limited = Limited::new(cursor, Limits::none());
    let entries: Result<Vec<ScSpecEntry>, _> = ScSpecEntry::read_xdr_iter(&mut limited).collect();
    entries.map_err(|e| anyhow!("failed decoding contract spec entries: {e}"))
}

/// A structured, JSON-friendly view over a contract's parsed interface.
pub struct ContractSpec {
    pub functions: Vec<ScSpecFunctionV0>,
    pub structs: HashMap<String, ScSpecUdtStructV0>,
    pub unions: HashMap<String, ScSpecUdtUnionV0>,
    pub enums: HashMap<String, ScSpecUdtEnumV0>,
    pub error_enums: HashMap<String, ScSpecUdtErrorEnumV0>,
}

impl ContractSpec {
    pub fn from_entries(entries: Vec<ScSpecEntry>) -> Self {
        let mut spec = Self {
            functions: Vec::new(),
            structs: HashMap::new(),
            unions: HashMap::new(),
            enums: HashMap::new(),
            error_enums: HashMap::new(),
        };
        for entry in entries {
            match entry {
                ScSpecEntry::FunctionV0(f) => spec.functions.push(f),
                ScSpecEntry::UdtStructV0(s) => {
                    spec.structs.insert(s.name.to_utf8_string_lossy(), s);
                }
                ScSpecEntry::UdtUnionV0(u) => {
                    spec.unions.insert(u.name.to_utf8_string_lossy(), u);
                }
                ScSpecEntry::UdtEnumV0(e) => {
                    spec.enums.insert(e.name.to_utf8_string_lossy(), e);
                }
                ScSpecEntry::UdtErrorEnumV0(e) => {
                    spec.error_enums.insert(e.name.to_utf8_string_lossy(), e);
                }
                ScSpecEntry::EventV0(_) => {
                    // Events aren't callable API surface; skip.
                }
            }
        }
        spec
    }

    pub fn function(&self, name: &str) -> Option<&ScSpecFunctionV0> {
        self.functions
            .iter()
            .find(|f| f.name.0.to_utf8_string_lossy() == name)
    }

    pub fn union_case(&self, union_name: &str, case_name: &str) -> Option<&ScSpecUdtUnionCaseV0> {
        self.unions.get(union_name).and_then(|u| {
            u.cases.iter().find(|c| match c {
                ScSpecUdtUnionCaseV0::VoidV0(v) => v.name.to_utf8_string_lossy() == case_name,
                ScSpecUdtUnionCaseV0::TupleV0(t) => t.name.to_utf8_string_lossy() == case_name,
            })
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "functions": self.functions.iter().map(function_to_json).collect::<Vec<_>>(),
            "structs": self.structs.values().map(struct_to_json).collect::<Vec<_>>(),
            "unions": self.unions.values().map(union_to_json).collect::<Vec<_>>(),
            "enums": self.enums.values().map(enum_to_json).collect::<Vec<_>>(),
            "error_enums": self.error_enums.values().map(error_enum_to_json).collect::<Vec<_>>(),
        })
    }
}

pub fn function_to_json(f: &ScSpecFunctionV0) -> serde_json::Value {
    serde_json::json!({
        "name": f.name.0.to_utf8_string_lossy(),
        "doc": f.doc.to_utf8_string_lossy(),
        "inputs": f.inputs.iter().map(|i| serde_json::json!({
            "name": i.name.to_utf8_string_lossy(),
            "doc": i.doc.to_utf8_string_lossy(),
            "type": type_def_to_json(&i.type_),
        })).collect::<Vec<_>>(),
        "output": f.outputs.first().map(type_def_to_json),
    })
}

fn struct_to_json(s: &ScSpecUdtStructV0) -> serde_json::Value {
    serde_json::json!({
        "name": s.name.to_utf8_string_lossy(),
        "doc": s.doc.to_utf8_string_lossy(),
        "fields": s.fields.iter().map(|f| serde_json::json!({
            "name": f.name.to_utf8_string_lossy(),
            "doc": f.doc.to_utf8_string_lossy(),
            "type": type_def_to_json(&f.type_),
        })).collect::<Vec<_>>(),
    })
}

fn union_to_json(u: &ScSpecUdtUnionV0) -> serde_json::Value {
    serde_json::json!({
        "name": u.name.to_utf8_string_lossy(),
        "doc": u.doc.to_utf8_string_lossy(),
        "cases": u.cases.iter().map(|c| match c {
            ScSpecUdtUnionCaseV0::VoidV0(v) => serde_json::json!({
                "name": v.name.to_utf8_string_lossy(),
                "doc": v.doc.to_utf8_string_lossy(),
                "kind": "void",
                "types": [],
            }),
            ScSpecUdtUnionCaseV0::TupleV0(t) => serde_json::json!({
                "name": t.name.to_utf8_string_lossy(),
                "doc": t.doc.to_utf8_string_lossy(),
                "kind": "tuple",
                "types": t.type_.iter().map(type_def_to_json).collect::<Vec<_>>(),
            }),
        }).collect::<Vec<_>>(),
    })
}

fn enum_to_json(e: &ScSpecUdtEnumV0) -> serde_json::Value {
    serde_json::json!({
        "name": e.name.to_utf8_string_lossy(),
        "doc": e.doc.to_utf8_string_lossy(),
        "cases": e.cases.iter().map(|c| serde_json::json!({
            "name": c.name.to_utf8_string_lossy(),
            "doc": c.doc.to_utf8_string_lossy(),
            "value": c.value,
        })).collect::<Vec<_>>(),
    })
}

fn error_enum_to_json(e: &ScSpecUdtErrorEnumV0) -> serde_json::Value {
    serde_json::json!({
        "name": e.name.to_utf8_string_lossy(),
        "doc": e.doc.to_utf8_string_lossy(),
        "cases": e.cases.iter().map(|c| serde_json::json!({
            "name": c.name.to_utf8_string_lossy(),
            "doc": c.doc.to_utf8_string_lossy(),
            "value": c.value,
        })).collect::<Vec<_>>(),
    })
}

pub fn type_def_to_json(t: &ScSpecTypeDef) -> serde_json::Value {
    use serde_json::json;
    match t {
        ScSpecTypeDef::Val => json!({"kind": "val"}),
        ScSpecTypeDef::Bool => json!({"kind": "bool"}),
        ScSpecTypeDef::Void => json!({"kind": "void"}),
        ScSpecTypeDef::Error => json!({"kind": "error"}),
        ScSpecTypeDef::U32 => json!({"kind": "u32"}),
        ScSpecTypeDef::I32 => json!({"kind": "i32"}),
        ScSpecTypeDef::U64 => json!({"kind": "u64"}),
        ScSpecTypeDef::I64 => json!({"kind": "i64"}),
        ScSpecTypeDef::Timepoint => json!({"kind": "timepoint"}),
        ScSpecTypeDef::Duration => json!({"kind": "duration"}),
        ScSpecTypeDef::U128 => json!({"kind": "u128"}),
        ScSpecTypeDef::I128 => json!({"kind": "i128"}),
        ScSpecTypeDef::U256 => json!({"kind": "u256"}),
        ScSpecTypeDef::I256 => json!({"kind": "i256"}),
        ScSpecTypeDef::Bytes => json!({"kind": "bytes"}),
        ScSpecTypeDef::String => json!({"kind": "string"}),
        ScSpecTypeDef::Symbol => json!({"kind": "symbol"}),
        ScSpecTypeDef::Address => json!({"kind": "address"}),
        ScSpecTypeDef::MuxedAddress => json!({"kind": "muxed_address"}),
        ScSpecTypeDef::Option(o) => json!({"kind": "option", "value": type_def_to_json(&o.value_type)}),
        ScSpecTypeDef::Result(r) => json!({
            "kind": "result",
            "ok": type_def_to_json(&r.ok_type),
            "error": type_def_to_json(&r.error_type),
        }),
        ScSpecTypeDef::Vec(v) => json!({"kind": "vec", "element": type_def_to_json(&v.element_type)}),
        ScSpecTypeDef::Map(m) => json!({
            "kind": "map",
            "key": type_def_to_json(&m.key_type),
            "value": type_def_to_json(&m.value_type),
        }),
        ScSpecTypeDef::Tuple(t) => json!({
            "kind": "tuple",
            "elements": t.value_types.iter().map(type_def_to_json).collect::<Vec<_>>(),
        }),
        ScSpecTypeDef::BytesN(n) => json!({"kind": "bytesN", "n": n.n}),
        ScSpecTypeDef::Udt(u) => json!({"kind": "udt", "name": u.name.to_utf8_string_lossy()}),
    }
}
