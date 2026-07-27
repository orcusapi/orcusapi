# OrcusAPI

Turns any Soroban smart contract into a plain HTTP/JSON API.

Point it at a contract's **WASM hash** (via config), and it reads the
contract's interface (functions, structs, enums, unions) straight out of
the WASM's `contractspecv0` custom section, then lets you call those
functions over HTTP instead of building Soroban transactions and XDR by
hand.

## How it fits together

Each running instance of the proxy serves one deployed contract, fixed at
startup via env vars — run one instance per contract you want to expose.

A WASM hash only identifies *code* (the interface); calling a function
requires a specific *deployed instance* of that code (a `C...` contract
ID), since many contracts can share the same WASM. So:

- `CONTRACT_WASM_HASH` (env) selects which interface/spec this instance
  serves.
- `CONTRACT_ID` (env) selects which deployed instance `/invoke` actually
  calls. It must be an instance running the code at `CONTRACT_WASM_HASH`.

Two ways to submit a call, chosen per-request:

- **Unsigned (default)** — the proxy simulates the call and hands back an
  unsigned transaction XDR for you to sign and submit yourself. The proxy
  never sees or stores a private key.
- **Signed** — pass `"sign": true` and a `secret_key`; the proxy signs,
  submits, and polls until the transaction lands, returning the result.
  The secret key is used in-memory for that one request only.

## Running it

```bash
cp .env.example .env
# edit .env: set SOROBAN_RPC_URL / SOROBAN_NETWORK_PASSPHRASE / CONTRACT_WASM_HASH / CONTRACT_ID
cargo run
```

Config is via environment variables (see `.env.example`):

| Variable | Required | Description |
|---|---|---|
| `SOROBAN_RPC_URL` | yes | Soroban RPC endpoint, e.g. `https://soroban-testnet.stellar.org` |
| `SOROBAN_NETWORK_PASSPHRASE` | yes | Must match the network the RPC endpoint serves |
| `CONTRACT_WASM_HASH` | yes | Hex-encoded hash of the contract WASM this instance exposes as an API |
| `CONTRACT_ID` | yes | C... address of the deployed contract instance `/invoke` calls are sent to |
| `BIND_ADDR` | no (default `0.0.0.0:8080`) | HTTP listen address |
| `REQUEST_TIMEOUT_SECS` | no (default `30`) | Timeout for upstream RPC calls |

Startup fails fast with a clear error if `CONTRACT_WASM_HASH` isn't valid
hex for a 32-byte hash, or `CONTRACT_ID` isn't a valid `C...` strkey. To
serve a different contract, change the env vars and restart (or run a
second instance on a different port/env file).

## Endpoints

### `GET /health`
Liveness check.

### `GET /network`
Proxies the RPC's `getNetwork` — passphrase, protocol version, friendbot URL.

### `GET /spec`
Full parsed contract interface: functions, structs, unions, enums, error
enums, each with their field/parameter types.

### `GET /functions`
Just the function list from the spec above.

### `POST /invoke/{function_name}`

Request body:

```jsonc
{
  "source_account": "G...",     // account that pays the fee / is the tx source
  "args": { "param_name": ... },// named args, keyed by the spec's parameter names
  "sign": false,                 // false (default) = simulate only, return unsigned XDR
  "secret_key": "S..."           // required only when sign=true; never persisted
}
```

Unsigned response:

```json
{
  "status": "simulated",
  "simulated_result": 5,
  "min_resource_fee": "12712",
  "transaction_xdr": "AAAAAgAAAAD...",
  "network_passphrase": "Test SDF Network ; September 2015"
}
```

Signed response (after submit + confirmation):

```json
{
  "status": "SUCCESS",
  "hash": "f4cfa70e...",
  "return_value": 10
}
```

## Argument conversion

Arguments and return values are converted between plain JSON and Soroban's
`ScVal` using the types declared in the contract's spec:

| Spec type | JSON |
|---|---|
| `bool`, `u32`, `i32` | native JSON bool/number |
| `u64`, `i64`, `u128`, `i128` | number **or** decimal string (string recommended to avoid precision loss) |
| `string`, `symbol` | string |
| `bytes`, `bytesN` | hex string (`0x`-prefixed or not) or array of byte values |
| `address` | `G...` or `C...` strkey string |
| `option<T>` | `null` or a value of type `T` |
| `vec<T>` | JSON array |
| `map<K,V>` | JSON object (keys stringified per `K`) |
| `tuple<...>` | JSON array, positional |
| struct (named fields) | JSON object keyed by field name |
| struct (tuple fields) | JSON array, positional |
| enum (fieldless) | case name string, or its integer value |
| union | case name string (void case), or `{"CaseName": [args...]}` |

Anything not covered above (`u256`/`i256`, exotic address kinds, or just to
bypass conversion entirely) can be supplied as `{"__xdr": <value>}` for any
single argument, where `<value>` is the raw serde-JSON form of the `ScVal`
itself.

Return values follow the same rendering in reverse; 64-bit-and-wider
integers are rendered as decimal strings.

## Notes / limitations

- Contract calls whose simulation requires restoring archived ledger state
  first are reported as an error rather than auto-restored.
- The signed flow polls `getTransaction` for up to ~30 seconds; slower
  confirmations return a timeout even though the transaction may still land.
- CORS is wide open by default (`CorsLayer::permissive()`); tighten this in
  `src/main.rs` before exposing the proxy publicly.
| `REQUEST_TIMEOUT_SECS` | no (default `30`) | Timeout for upstream RPC calls |

Startup fails fast with a clear error if `CONTRACT_WASM_HASH` isn't valid
hex for a 32-byte hash. To serve a different contract, change the env var
and restart (or run a second instance on a different port/env file).

## Endpoints

### `GET /health`
Liveness check.

### `GET /network`
Proxies the RPC's `getNetwork` — passphrase, protocol version, friendbot URL.

### `GET /spec`
Full parsed contract interface: functions, structs, unions, enums, error
enums, each with their field/parameter types.

### `GET /functions`
Just the function list from the spec above.

### `POST /invoke/{function_name}`

Request body:

```jsonc
{
  "contract_id": "C...",        // the deployed contract instance to call
  "source_account": "G...",     // account that pays the fee / is the tx source
  "args": { "param_name": ... },// named args, keyed by the spec's parameter names
  "sign": false,                 // false (default) = simulate only, return unsigned XDR
  "secret_key": "S..."           // required only when sign=true; never persisted
}
```

Unsigned response:

```json
{
  "status": "simulated",
  "simulated_result": 5,
  "min_resource_fee": "12712",
  "transaction_xdr": "AAAAAgAAAAD...",
  "network_passphrase": "Test SDF Network ; September 2015"
}
```

Signed response (after submit + confirmation):

```json
{
  "status": "SUCCESS",
  "hash": "f4cfa70e...",
  "return_value": 10
}
```

## Argument conversion

Arguments and return values are converted between plain JSON and Soroban's
`ScVal` using the types declared in the contract's spec:

| Spec type | JSON |
|---|---|
| `bool`, `u32`, `i32` | native JSON bool/number |
| `u64`, `i64`, `u128`, `i128` | number **or** decimal string (string recommended to avoid precision loss) |
| `string`, `symbol` | string |
| `bytes`, `bytesN` | hex string (`0x`-prefixed or not) or array of byte values |
| `address` | `G...` or `C...` strkey string |
| `option<T>` | `null` or a value of type `T` |
| `vec<T>` | JSON array |
| `map<K,V>` | JSON object (keys stringified per `K`) |
| `tuple<...>` | JSON array, positional |
| struct (named fields) | JSON object keyed by field name |
| struct (tuple fields) | JSON array, positional |
| enum (fieldless) | case name string, or its integer value |
| union | case name string (void case), or `{"CaseName": [args...]}` |

Anything not covered above (`u256`/`i256`, exotic address kinds, or just to
bypass conversion entirely) can be supplied as `{"__xdr": <value>}` for any
single argument, where `<value>` is the raw serde-JSON form of the `ScVal`
itself.

Return values follow the same rendering in reverse; 64-bit-and-wider
integers are rendered as decimal strings.

## Notes / limitations

- Contract calls whose simulation requires restoring archived ledger state
  first are reported as an error rather than auto-restored.
- The signed flow polls `getTransaction` for up to ~30 seconds; slower
  confirmations return a timeout even though the transaction may still land.
- CORS is wide open by default (`CorsLayer::permissive()`); tighten this in
  `src/main.rs` before exposing the proxy publicly.
  `src/main.rs` before exposing the proxy publicly.
