# Hyperliquid signing reference for `oppen-hl`

This is the reference `crates/oppen-hl` is implemented and tested against. Every rule cites the official source it was read from. Nothing here is inferred from memory; where a source does not settle a question it is listed under [Open questions](#open-questions).

Sources, pinned on 2026-09-03:

| Tag | Source | Pin |
|---|---|---|
| DOC-SIGN | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/signing> | live page |
| DOC-EXCH | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint> | live page |
| DOC-NONCE | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets> | live page |
| DOC-ASSET | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids> | live page |
| DOC-TICK | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size> | live page |
| DOC-NOTATION | <https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/notation> | live page |
| PY-SIGNING | <https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/utils/signing.py> | `2fdb18f9517675ea03695a0962bd19eece9c83f0` (v0.24.0) |
| PY-EXCHANGE | <https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/exchange.py> | same |
| PY-TYPES | <https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/utils/types.py> | same |
| PY-TESTS | <https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py> | same |
| PY-CONST | <https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/hyperliquid/utils/constants.py> | same |
| RS-SIG | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/signature/create_signature.rs> | `aac75585daf12d0a3761126cc7da7a5e035b5853` (v0.6.0) |
| RS-AGENT | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/signature/agent.rs> | same |
| RS-EIP712 | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/eip712.rs> | same |
| RS-HELPERS | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/helpers.rs> | same |
| RS-ACTIONS | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/exchange/actions.rs> | same |
| RS-ORDER | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/exchange/order.rs> | same |
| RS-CANCEL | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/exchange/cancel.rs> | same |
| RS-BUILDER | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/exchange/builder.rs> | same |
| RS-EXCH | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/exchange/exchange_client.rs> | same |
| RS-CONSTS | <https://github.com/hyperliquid-dex/hyperliquid-rust-sdk/blob/master/src/consts.rs> | same |

Third-party facts the rust-SDK vectors depend on: alloy `Signature` byte layout (<https://github.com/alloy-rs/core/blob/main/crates/primitives/src/signature/sig.rs>), alloy `FixedBytes` serde (<https://github.com/alloy-rs/core/blob/main/crates/primitives/src/bits/serde.rs>), rmp-serde default config (<https://github.com/3Hren/msgpack-rust/blob/master/rmp-serde/src/config.rs>, <https://github.com/3Hren/msgpack-rust/blob/master/rmp-serde/src/encode.rs>), eth-utils `to_hex` (<https://github.com/ethereum/eth-utils/blob/main/eth_utils/conversions.py>).

Test vectors: `crates/oppen-hl/tests/vectors/signing.json` (40 vectors, all copied verbatim from PY-TESTS, RS-SIG and RS-EXCH; see [Test vectors](#test-vectors)).

---

## 1. Two signing schemes

DOC-SIGN lists as the first common error: "Not realizing that there are two signing schemes (the Python SDK methods are `sign_l1_action` vs `sign_user_signed_action`)."

| Scheme | Used by | Signer | Hash input |
|---|---|---|---|
| **L1 action** (`sign_l1_action`) | order, cancel, cancelByCloid, modify, batchModify, scheduleCancel, updateLeverage, updateIsolatedMargin, createSubAccount, subAccountTransfer, vaultTransfer, setReferrer, claimRewards, evmUserModify, spotUser, ... | agent (API) wallet or master | msgpack(action) + nonce + vault marker + optional expiresAfter → keccak → phantom `Agent` EIP-712 struct on chain id 1337 |
| **User-signed** (`sign_user_signed_action`) | approveAgent, approveBuilderFee, usdSend, spotSend, withdraw3, usdClassTransfer, sendAsset, tokenDelegate, cDeposit/cWithdraw, convertToMultiSigUser, multiSig envelope | the account itself (master) | EIP-712 typed data on the wallet's real chain id, `primaryType = "HyperliquidTransaction:<Name>"` |

The action `type` values above are the python SDK's (PY-EXCHANGE) and DOC-EXCH section headings. For oppen, D5 means every user-signed action goes through WalletConnect; the in-app signer only ever produces L1 signatures with the agent wallet.

DOC-SIGN also states the failure mode for any signing bug: the L1 recovers a *different* address and rejects with `"L1 error: User or API Wallet 0x0123... does not exist."` or `Must deposit before performing actions. User: 0x123...` — the recovered address "also changes for different inputs", so a wrong signature is indistinguishable from an unknown user. DOC-SIGN's fifth error: a local `recover` that succeeds proves nothing, because "the payload for recover signer is constructed based on the action and does not necessarily match."

---

## 2. L1 action signing

### 2.1 Action hash

PY-SIGNING `action_hash` (lines 174–185), byte-for-byte:

```
data  = msgpack.packb(action)
data += nonce.to_bytes(8, "big")
if vault_address is None: data += b"\x00"
else:                     data += b"\x01" + bytes.fromhex(vault_address without 0x)   # 20 raw bytes
if expires_after is not None:
    data += b"\x00" + expires_after.to_bytes(8, "big")
hash = keccak(data)
```

RS-EXCH `Actions::hash` (lines 88–99) does the same with `rmp_serde::to_vec_named(self)`, `timestamp.to_be_bytes()`, then `1` + address bytes or `0`. **The rust SDK 0.6.0 `hash` has no `expiresAfter` branch** — use the python SDK as the reference for that field.

Rules that follow:

- `nonce` is an unsigned 64-bit big-endian integer appended raw, not msgpacked.
- The vault marker byte is **always** present (`0x00` or `0x01`).
- `expiresAfter` is encoded as a second `0x00` byte followed by 8 BE bytes, and only when present. DOC-EXCH "Expires After": "Some actions support an optional field `expiresAfter` which is a timestamp in milliseconds after which the action will be rejected. User-signed actions such as Core USDC transfer do not support the `expiresAfter` field. Note that actions consume 5x the usual address-based rate limit when canceled due to a stale `expiresAfter` field." PY-EXCHANGE lines 134–136: "expires_after is not supported on user_signed actions (e.g. usd_transfer) and must be None in order for those actions to work."
- The vault address bytes are the sub-account or vault address. DOC-EXCH "Subaccounts and vaults": "Subaccounts and vaults do not have private keys. To perform actions on behalf of a subaccount or vault signing should be done by the master account and the vaultAddress field should be set to the address of the subaccount or vault."

### 2.2 Phantom agent and EIP-712 envelope

PY-SIGNING `construct_phantom_agent` (188–189) and `l1_payload` (192–214); RS-AGENT (11–31) is identical:

```
message     = { "source": "a" if mainnet else "b", "connectionId": <32-byte action hash> }
primaryType = "Agent"
types.Agent = [ {source: string}, {connectionId: bytes32} ]
domain      = { name: "Exchange", version: "1", chainId: 1337,
                verifyingContract: "0x0000000000000000000000000000000000000000" }
```

The digest is standard EIP-712: `keccak(0x19 0x01 || domainSeparator || hashStruct(Agent))` (RS-EIP712 lines 10–17). The signature is a plain secp256k1 signature over that digest with `v ∈ {27, 28}` (PY-SIGNING `sign_inner` 452–455 returns `{"r", "s", "v"}`; RS-EXCH `serialize_sig` 42–51 sends `v = 27 + y_parity`).

Chain id **1337 is constant on both networks**; the network is carried only by `source` (`"a"` mainnet / `"b"` testnet). Signing the same hash for the other network changes the signature (PY-TESTS `test_l1_action_signing_matches` has both).

### 2.3 What gets msgpacked — field order

DOC-SIGN error #2: "Not realizing that the order of fields matter for msgpack." msgpack maps are ordered byte sequences; the python SDK relies on dict insertion order and the rust SDK on serde struct-field order. The orders below are the ones the official SDKs produce and the vectors pin.

| Action | Key order | Source |
|---|---|---|
| `order` | `type`, `orders`, `grouping`, then `builder` **only if set** | PY-SIGNING `order_wires_to_order_action` 519–527; RS-ACTIONS `BulkOrder` 74–81 (`skip_serializing_if = "Option::is_none"`) |
| order wire | `a`, `b`, `p`, `s`, `r`, `t`, then `c` **only if set** | PY-SIGNING `order_request_to_order_wire` 505–516; RS-ORDER 33–50 |
| `t` limit | `{"limit": {"tif": ...}}` | PY-SIGNING 156–158; RS-ORDER 13–16, 26–31 |
| `t` trigger | `{"trigger": {"isMarket", "triggerPx", "tpsl"}}` in that order | PY-SIGNING 159–166; RS-ORDER 18–24 |
| `cancel` | `type`, `cancels: [{a, o}]` | DOC-EXCH "Cancel order(s)"; RS-CANCEL 10–16 |
| `cancelByCloid` | `type`, `cancels: [{asset, cloid}]` | DOC-EXCH "Cancel order(s) by cloid"; RS-CANCEL 24–28 |
| `scheduleCancel` | `type`, then `time` **only if set** (absent, not null) | PY-EXCHANGE 370–374; RS-ACTIONS 267–272 |
| `createSubAccount` | `type`, `name` | PY-EXCHANGE 456–459 |
| `subAccountTransfer` | `type`, `subAccountUser`, `isDeposit`, `usd` (integer) | PY-TESTS `test_sub_account_transfer_action` |
| `builder` | `{"b": <lowercased address>, "f": <int>}` | DOC-EXCH order body; PY-EXCHANGE 171–172 lowercases `b`; RS-BUILDER 3–10 |

The `fast` cancel flag: DOC-EXCH "Both `cancel` and `cancelByCloid` actions include an optional `fast` flag, encoded as `f` in the action. ... Note that `f` must be skipped if false, i.e. actions hashed with `f: false` will be rejected."

Implementation guidance for oppen: serialize actions from Rust structs with `#[serde(rename = ...)]` and `skip_serializing_if` exactly as RS-ORDER / RS-CANCEL / RS-ACTIONS do, through `rmp_serde::to_vec_named` (what RS-EXCH uses). Never route an action through `serde_json::Value` or any map type that sorts keys.

### 2.4 msgpack value types

- Prices, sizes and trigger prices are **strings** (`p`, `s`, `triggerPx`: DOC-EXCH order body; PY-SIGNING 509–510, 163; RS-ORDER 107, 120–121).
- Asset, oid, nonce-like fields (`time`), `usd`, `ntli`, `f` are **integers** (DOC-EXCH bodies; PY-TESTS `usd: 10`, `time: 123456789`; RS-CANCEL `oid: u64`).
- Booleans are booleans (`b`, `r`, `isMarket`, `isDeposit`).
- `cloid` is a **string**: `"0x"` + 32 hex chars = 16 bytes (PY-TYPES `Cloid._validate` 199–203: must start with `0x` and have exactly 32 hex chars; DOC-EXCH: "Client Order ID (cloid) is an optional 128 bit hex string, e.g. `0x1234567890abcdef1234567890abcdef`"). RS-HELPERS `uuid_to_hex_string` 46–54 lowercases.
- Addresses inside L1 actions are **lowercase hex strings** in the python SDK (PY-EXCHANGE 172, 1100, 1108; PY-SIGNING 282–283). DOC-SIGN error #4: "It is recommended to lowercase any address before signing and sending. Sometimes the field is parsed as bytes, causing it to be lowercased automatically across the network."
- Never emit a msgpack float. The SDKs convert every float to a string (`float_to_wire`) or to an int (`float_to_int`) before packing.

Caution on the rust SDK: alloy `Address` serializes as **raw bytes** under rmp-serde's default non-human-readable config (alloy `bits/serde.rs` `Serialize for FixedBytes`: `serialize_bytes` when `!is_human_readable`; rmp-serde `DefaultConfig::is_human_readable() -> false`, and `to_vec_named` → `write_named` → `Serializer::new` uses `DefaultConfig`). The rust vector `test_approve_builder_fee_hash` is hashed that way. oppen must serialize addresses as strings, matching the python SDK and DOC-SIGN.

---

## 3. User-signed actions

### 3.1 Envelope

PY-SIGNING `user_signed_payload` (217–237) and `sign_user_signed_action` (247–253); RS-ACTIONS `eip_712_domain` (14–21):

```
domain      = { name: "HyperliquidSignTransaction", version: "1",
                chainId: int(action.signatureChainId, 16),
                verifyingContract: "0x0000000000000000000000000000000000000000" }
primaryType = "HyperliquidTransaction:<Name>"
types       = { primaryType: [...per-action fields...], EIP712Domain: [name, version, chainId, verifyingContract] }
message     = the action (fields named exactly as in `types`)
```

Two chain-related fields, with different jobs (PY-SIGNING 248–249, verbatim comment): "signatureChainId is the chain used by the wallet to sign and can be any chain. hyperliquidChain determines the environment and prevents replaying an action on a different chain."

- `hyperliquidChain` is `"Mainnet"` or `"Testnet"` and is part of the signed message (first field of every type list). DOC-EXCH: `"hyperliquidChain": "Mainnet" (on testnet use "Testnet" instead)`.
- `signatureChainId` is a hex string in the action and must equal the EIP-712 domain `chainId` used by the wallet. DOC-EXCH: `"signatureChainId": the id of the chain used when signing in hexadecimal format; e.g. "0xa4b1" for Arbitrum`. The python SDK hardcodes `"0x66eee"` (Arbitrum Sepolia, 421614) on **both** networks (PY-SIGNING 250). The rust tests use `421614` with both `"Mainnet"` and `"Testnet"` (RS-EXCH `test_approve_builder_fee_signing`). DOC-EXCH's spotSend example typed data uses `chainId: 42161` with `hyperliquidChain: "Mainnet"`.
- `signatureChainId` is **not** an EIP-712 message field; it only selects the domain chain id. It is still sent in the JSON action.
- The `nonce` (or `time`) field inside the action must equal the outer request `nonce`: DOC-EXCH approveAgent/approveBuilderFee/usdClassTransfer: "must match nonce in outer request body".

For oppen's WalletConnect ceremony (D5): build the typed-data JSON exactly as `user_signed_payload` does (including the explicit `EIP712Domain` type), set `hyperliquidChain` from the network switch, and set `signatureChainId` to the chain the wallet session actually signs on — that is what the field means per PY-SIGNING 248. The docs' example for mainnet is `0xa4b1` (Arbitrum One); the SDKs' default is `0x66eee` (Arbitrum Sepolia). Whether the L1 restricts accepted chain ids is not stated in any source read — see Open questions.

### 3.2 Per-action primary types and field lists

From PY-SIGNING (type lists 81–153 and the `sign_*` wrappers 332–449) and RS-ACTIONS type-hash strings, which agree:

| primaryType | Fields (in order) | Source |
|---|---|---|
| `HyperliquidTransaction:ApproveAgent` | `hyperliquidChain string`, `agentAddress address`, `agentName string`, `nonce uint64` | PY-SIGNING 412–424; RS-ACTIONS 119 |
| `HyperliquidTransaction:ApproveBuilderFee` | `hyperliquidChain string`, `maxFeeRate string`, `builder address`, `nonce uint64` | PY-SIGNING 427–439; RS-ACTIONS 285 |
| `HyperliquidTransaction:UsdSend` | `hyperliquidChain string`, `destination string`, `amount string`, `time uint64` | PY-SIGNING 81–86; RS-ACTIONS 48 |
| `HyperliquidTransaction:SpotSend` | `hyperliquidChain string`, `destination string`, `token string`, `amount string`, `time uint64` | PY-SIGNING 88–94; RS-ACTIONS 176 |
| `HyperliquidTransaction:Withdraw` (action type `withdraw3`) | `hyperliquidChain string`, `destination string`, `amount string`, `time uint64` | PY-SIGNING 96–101, 352–359; RS-ACTIONS 147 |
| `HyperliquidTransaction:UsdClassTransfer` | `hyperliquidChain string`, `amount string`, `toPerp bool`, `nonce uint64` | PY-SIGNING 103–108, 362–369 |
| `HyperliquidTransaction:SendAsset` | `hyperliquidChain string`, `destination string`, `sourceDex string`, `destinationDex string`, `token string`, `amount string`, `fromSubAccount string`, `nonce uint64` | PY-SIGNING 110–119; RS-ACTIONS 222 |
| `HyperliquidTransaction:TokenDelegate` | `hyperliquidChain string`, `validator address`, `wei uint64`, `isUndelegate bool`, `nonce uint64` | PY-SIGNING 135–141 |
| `HyperliquidTransaction:UserDexAbstraction` | `hyperliquidChain string`, `user address`, `enabled bool`, `nonce uint64` | PY-SIGNING 121–126 |
| `HyperliquidTransaction:UserSetAbstraction` | `hyperliquidChain string`, `user address`, `abstraction string`, `nonce uint64` | PY-SIGNING 128–133 |
| `HyperliquidTransaction:ConvertToMultiSigUser` | `hyperliquidChain string`, `signers string`, `nonce uint64` | PY-SIGNING 143–147 |
| `HyperliquidTransaction:SendMultiSig` | `hyperliquidChain string`, `multiSigActionHash bytes32`, `nonce uint64` | PY-SIGNING 149–153, 315–329 |

Field typing matters for the hash: `string` fields are keccak'd as the exact text (case included — RS-SIG `test_sign_usd_transfer_action` signs a mixed-case `destination` string), `address` fields are ABI-encoded 20 bytes (case-insensitive), `uint64`/`bool` are ABI words (RS-ACTIONS `struct_hash` impls).

### 3.3 approveAgent specifics

- Action body (DOC-EXCH "Approve an API wallet"): `type: "approveAgent"`, `hyperliquidChain`, `signatureChainId`, `agentAddress`, `agentName`, `nonce`. `agentName`: "Optional name for the API wallet. An account can have 1 unnamed approved wallet and up to 3 named ones. And additional 2 named agents are allowed per subaccount. A custom expiration can be set by appending `valid_until {timestamp}` after the name. The expiration can be at most 180 days in the future".
- Unnamed agent: the python SDK **signs** with `agentName: ""` and then **deletes** `agentName` from the posted action (PY-EXCHANGE 640–648). The rust SDK hashes `agent_name.unwrap_or("")` (RS-ACTIONS 122) and serializes the `Option` as JSON `null`. Both sign the empty string.
- The agent key is generated client-side (`"0x" + secrets.token_hex(32)`, PY-EXCHANGE 636) and only its address leaves the process — matches oppen invariant 2.
- DOC-NONCE: "API wallets are only used to sign. To query the account data associated with a master or sub-account, you must pass in the actual address of that account." And on pruning: an unnamed agent is deregistered when a new unnamed one is approved; a named one when the same name is re-approved; wallets also expire or are pruned when the account has no funds. "it is **strongly** suggested to not reuse their addresses ... previously signed actions can be replayed once the nonce set is pruned."

### 3.4 approveBuilderFee specifics

DOC-EXCH "Approve a builder fee": `type: "approveBuilderFee"`, `hyperliquidChain`, `signatureChainId`, `maxFeeRate` ("the maximum allowed builder fee rate as a percent string; e.g. "0.001%""), `builder` (42-char hex), `nonce`. Python builds the dict as `{maxFeeRate, builder, nonce, type}` (PY-EXCHANGE 662) — irrelevant for EIP-712 (types define order) but shows JSON key order does not matter for user-signed actions.

On orders, the builder fee is attached per order action: `builder: {"b": address, "f": Number}` where "f is the size of the fee in tenths of a basis point e.g. if f is 10, 1bp of the order notional will be charged to the user and sent to the builder" (DOC-EXCH order body). The approved `maxFeeRate` caps it.

---

## 4. Float wire format — the trailing-zero footgun

DOC-TICK "Signing": "Note that if implementing signing, trailing zeroes should be removed." DOC-SIGN error #3: "Issues with trailing zeroes on numbers."

Both SDKs normalize floats to strings before they enter the action:

**PY-SIGNING `float_to_wire` (475–482)**
```
rounded = f"{x:.8f}"                       # fixed 8 decimals
if abs(float(rounded) - x) >= 1e-12: raise ValueError("float_to_wire causes rounding", x)
if rounded == "-0": rounded = "0"
normalized = Decimal(rounded).normalize()   # strips trailing zeros
return f"{normalized:f}"                    # plain notation, never exponent
```

**RS-HELPERS `float_to_string_for_hashing` (29–44)**, `WIRE_DECIMALS = 8`: format with 8 decimals, pop trailing `'0'`s, pop a trailing `'.'`, map `"-0"` to `"0"`.

Official normalization vectors (RS-HELPERS test 98–137, verbatim):

| input | wire string |
|---|---|
| `0.` | `"0"` |
| `-0.` | `"0"` |
| `0.00076000` | `"0.00076"` |
| `0.00000001` | `"0.00000001"` |
| `0.12345678` | `"0.12345678"` |
| `87654321.12345678` | `"87654321.12345678"` |
| `987654321.00000000` | `"987654321"` |
| `87654321.1234` | `"87654321.1234"` |
| `987654321.0` | `"987654321"` |

PY-TESTS `test_float_to_int_for_hashing` (8-decimal integer scaling, used for the `dummy` vectors and by `float_to_usd_int` with 6 decimals for `updateIsolatedMargin.ntli`, PY-EXCHANGE 413): `123123123123 → 12312312312300000000`, `0.00001231 → 1231`, `1.033 → 103300000`, `0.000012312312 → ValueError`.

Consequences the vectors make concrete:

- `100` is `"100"`, never `"100.0"` (PY-TESTS order vectors). `1670.1` is `"1670.1"`, `0.0147` is `"0.0147"` (phantom-agent vector).
- The rust SDK's own order vectors pass the literal `"2000.0"` straight into the wire struct (RS-EXCH 911, 942, 984, 988). That signature is over the un-normalized string; a correct normalizer produces `"2000"` and a **different hash**. Those vectors pin the hash/sign pipeline, not a valid price. `oppen-hl` must normalize *before* building the wire struct and must never accept a caller-supplied price string with trailing zeros.
- More than 8 decimals is an error in both SDKs, not a rounding.
- Exponent notation is never valid (`Decimal.normalize()` alone would give `1E+2`; the `:f` format is what prevents it).

Recommendation: carry prices/sizes as decimals (never `f64`) through validation, and emit the wire string with one function that is property-tested against these vectors: fixed-8 → strip zeros → strip dot → `"-0"`→`"0"`, error above 8 decimals.

---

## 5. Order wire format

DOC-EXCH "Place an order", request body `action`:

```
{
  "type": "order",
  "orders": [{
    "a": Number,          // asset
    "b": Boolean,         // isBuy
    "p": String,          // price
    "s": String,          // size
    "r": Boolean,         // reduceOnly
    "t": { "limit": { "tif": "Alo" | "Ioc" | "Gtc" } }
       or { "trigger": { "isMarket": Boolean, "triggerPx": String, "tpsl": "tp" | "sl" } },
    "c": Cloid (optional) // client order id
  }],
  "grouping": "na" | "normalTpsl" | "positionTpsl",
  "builder": Optional({"b": "address", "f": Number})
}
```

TIF semantics (DOC-EXCH): "ALO (add liquidity only, i.e. "post only") will be canceled instead of immediately matching. IOC (immediate or cancel) will have the unfilled part canceled instead of resting. GTC (good til canceled) orders have no special behavior." Wire spelling is title-case `"Alo" | "Ioc" | "Gtc"` (DOC-EXCH body; PY-SIGNING `Tif` line 13). DOC-NOTATION spells the concepts `GTC/ALO/IOC`; do not send those.

`tpsl` is `"tp"` or `"sl"` (PY-SIGNING `Tpsl` line 14). `grouping` values per DOC-EXCH are `"na"`, `"normalTpsl"`, `"positionTpsl"`; the spec's attached TP/SL uses `positionTpsl`. PY-SIGNING line 45–46 additionally defines an undocumented `PriorityGrouping = {"p": int}` variant — see Open questions.

Modify (`type: "modify"` / `"batchModify"`) reuses the same order wire under an `order` key plus `oid: Number | Cloid` (DOC-EXCH "Modify an order"); cancels use `{a, o}` and `{asset, cloid}` (section 2.3).

Response shapes (DOC-EXCH): `statuses[]` entries are `{"resting": {"oid"}}`, `{"filled": {"totalSz", "avgPx", "oid"}}` or `{"error": "..."}` — e.g. `"Order must have minimum value of $10."`.

---

## 6. Asset index rule

DOC-ASSET and DOC-EXCH "Asset":

- **Perps**: `asset` = index of the coin in the `meta` response `universe`. "E.g. `BTC = 0` on mainnet."
- **Spot**: `asset = 10000 + spotInfo["index"]` where `spotInfo` is the entry in `spotMeta.universe`. "when submitting an order for `PURR/USDC`, the asset that should be used is `10000`".
- **Builder-deployed perps**: `100000 + perp_dex_index * 10000 + index_in_meta`; "`test:ABC` on testnet has `perp_dex_index = 1`, `index_in_meta = 0`, `asset = 110000`. Note that builder-deployed perps always have name in the format `{dex}:{coin}`."
- **Outcomes**: `encoding = 10 * outcome + side` (side 0 or 1); asset id `100_000_000 + encoding`; coin `#<encoding>`, token `+<encoding>`.
- "Note that spot ID is different from token ID, and that mainnet and testnet have different asset IDs." HYPE: mainnet token 150 / spot 107; testnet token 1105 / spot 1035.

The python SDK detects spot with `asset >= 10_000` (PY-EXCHANGE 127) to pick 8 vs 6 price decimals; a builder-perp id (≥ 100000) also satisfies that test. oppen should classify assets from the meta tables it loaded, not from a threshold.

---

## 7. Tick and lot size (validation before signing)

DOC-TICK, verbatim rules:

- "Prices can have up to 5 significant figures, but no more than `MAX_DECIMALS - szDecimals` decimal places where `MAX_DECIMALS` is 6 for perps and 8 for spot. Integer prices are always allowed, regardless of the number of significant figures. E.g. `123456` is a valid price even though `12345.6` is not."
- "Sizes are rounded to the `szDecimals` of that asset. For example, if `szDecimals = 3` then `1.001` is a valid size but `1.0001` is not."
- Perp examples: `1234.5` valid, `1234.56` not; `0.001234` valid, `0.0012345` not; with `szDecimals = 1`, `0.01234` valid, `0.012345` not.
- Spot example: `0.0001234` valid if `szDecimals` is 0 or 1, not if greater than 2.

PY-EXCHANGE `_slippage_price` (129–132) is the SDK's market-order price computation: `px *= 1 ± slippage`, then `round(float(f"{px:.5g}"), (6 if perp else 8) - szDecimals)`. Spec item 8 requires the same rounding in oppen's validation layer, then `float_to_wire`.

---

## 8. Nonces

DOC-NONCE, verbatim:

- "On Hyperliquid, the 100 highest nonces are stored per address. Every new transaction must have nonce larger than the smallest nonce in this set and also never have been used before. Nonces are tracked per signer, which is the user address if signed with private key of the address, or the agent address if signed with an API wallet."
- "Nonces must be within `(T - 2 days, T + 1 day)`, where `T` is the unix millisecond timestamp on the block of the transaction."
- "a single API wallet signing for a user, vault, or subaccount all share the same nonce set." and "If users want to use multiple subaccounts in parallel, it would easier to generate two separate API wallets under the master account, and use one API wallet for each subaccount." (= spec D1 / item 7).
- Suggested structure: batch orders and cancels every 0.1 s; "It is recommended to batch IOC and GTC orders separately from ALO orders because ALO order-only batches are prioritized by the validators."; "fetch and increment an atomic counter that ensures a unique nonce for the address. The atomic counter can be fast-forwarded to current unix milliseconds if needed."

SDK behaviour: python uses `int(time.time() * 1000)` per action (PY-SIGNING 501–502); rust uses a process-global `AtomicU64` seeded with now-ms, `fetch_add(1)`, and jumps to now-ms if it has fallen more than 300 s behind (RS-HELPERS 15–27). Nonce is the outer `nonce` field, and for user-signed actions also the `nonce`/`time` inside the action (section 3.1). DOC-EXCH: "Recommended to use the current timestamp in milliseconds".

---

## 9. Request envelope

`POST /exchange`, `Content-Type: application/json` (DOC-EXCH). Base URLs: mainnet `https://api.hyperliquid.xyz`, testnet `https://api.hyperliquid-testnet.xyz` (PY-CONST; RS-CONSTS).

PY-EXCHANGE `_post_action` (101–110):
```
{ "action": action, "nonce": nonce, "signature": {"r", "s", "v"},
  "vaultAddress": vault_address  (None for usdClassTransfer and sendAsset),
  "expiresAfter": expires_after }
```
Rust `ExchangePayload` (RS-EXCH 53–61): `action`, `signature`, `nonce`, `vaultAddress`; 0.6.0 has no `expiresAfter`.

Signature encoding: python sends `r`/`s` as `eth_utils.to_hex(int)`, which for an integer is Python's `hex()` ("Trims leading zeros", eth-utils `conversions.py` `to_hex`), i.e. minimal-length hex without zero padding (the PY-TESTS values range from 62 to 64 hex chars — compare as integers, and pad to 32 bytes before recovering), `v` as 27/28. Rust sends `v = 27 + y_parity` (RS-EXCH 49). The alloy `Signature` string used in the rust vectors is `0x || r(32) || s(32) || v_byte` with `v_byte = 27 + y_parity` (alloy `sig.rs` `as_bytes`/`v_byte`).

Sub-account routing (spec D1): sign with the sub-account's agent wallet and set `vaultAddress` to the sub-account address (DOC-EXCH "Subaccounts and vaults"). `usdClassTransfer` for a sub-account instead encodes it in the amount string: `"1" subaccount:0x...` (DOC-EXCH "Transfer from Spot account to Perp account"; PY-EXCHANGE 476–478), with `vaultAddress` omitted.

---

## 10. Divergences and quirks to remember

1. Rust SDK 0.6.0 `Actions::hash` ignores `expiresAfter`; python is the reference for it. No official vector exercises `expiresAfter`.
2. Rust SDK vectors carry un-normalized `"2000.0"` strings (section 4).
3. Rust SDK msgpacks alloy `Address` as raw bytes (section 2.4); python sends lowercase strings. Follow python.
4. Python `sign_user_signed_action` hardcodes `signatureChainId = "0x66eee"` and overwrites whatever the caller set (PY-SIGNING 250).
5. Python `float_to_wire` compares `rounded == "-0"` but `rounded` is `"-0.00000000"` at that point, so a negative zero reaches `Decimal` and formats as `"-0"`; rust returns `"0"`. Sizes and prices are never negative, so this cannot occur on a validated path.
6. Python `approve_agent` signs `agentName: ""` and then deletes the key before posting (section 3.3).
7. `scheduleCancel` without `time` omits the key entirely; with `time` it is an integer ms (PY-TESTS both variants).
8. Python `Grouping` includes an undocumented `{"p": int}` priority variant (PY-SIGNING 45).
9. `f` (fast cancel) must be absent when false (DOC-EXCH).

---

## Test vectors

`crates/oppen-hl/tests/vectors/signing.json` — a JSON array; each object has `name`, `source_url`, `source_commit`, `network`, `kind` (`l1` | `user`), `private_key`, `action`, `nonce`, `vault_address`, `expires_after`, `expected`, plus `primary_type`/`eip712_types` for user-signed vectors, `connection_id` when the source supplies the hash directly, and `notes` where the source needs an explanation. Every `expected` value is copied verbatim from the cited test; nothing is derived.

| Group | Count | Source | Expected form |
|---|---|---|---|
| python L1 (`dummy`, order, order+cloid, vault, tpsl, createSubAccount, subAccountTransfer, scheduleCancel ×2) × mainnet/testnet | 20 | PY-TESTS | `r`, `s` (minimal hex), `v` |
| python phantom-agent connection id | 1 | PY-TESTS | `connection_id` |
| python user-signed (UsdSend, Withdraw) | 2 | PY-TESTS | `r`, `s`, `v` |
| rust phantom-agent sign from a given connection id × mainnet/testnet | 2 | RS-SIG | 65-byte `signature_hex` |
| rust user-signed (UsdSend, Withdraw3, ApproveBuilderFee ×2) | 4 | RS-SIG, RS-EXCH | `signature_hex` |
| rust L1 (limit order, order+cloid, tpsl tp/sl, cancel, claimRewards) × mainnet/testnet | 12 | RS-EXCH | `signature_hex` |
| rust msgpack action hash (approveBuilderFee, bytes-address caveat) | 1 | RS-EXCH | `action_hash` |

Total: 40. Vectors whose `private_key` is `null` test hashing only. The two python test functions that are not signing vectors (`test_float_to_int_for_hashing`, `test_multi_sig_user_set_abstraction_payload_uses_wire_enum`) and the rust `test_send_asset_signing` (asserts only inequality) are not in the file; the float table lives in section 4.

The transcription was validated by running the official `signing.py` (pinned commit) over the JSON: all 40 entries reproduce their expected values, including the rust vectors re-derived with python primitives (the bytes-address hash vector reproduces only when `builder` is packed as 20 raw bytes, confirming the caveat).

---

## Open questions

1. **`expiresAfter` has no official vector.** The encoding is read from PY-SIGNING 182–184 only. oppen should generate a testnet round-trip (send an order with `expiresAfter` in the past and expect the rejection) before relying on it.
2. **Accepted `signatureChainId` values.** No source read states whether the L1 validates the chain id against an allowlist; the docs give `0xa4b1` as an example and the SDKs use `0x66eee` on both networks. Confirm on testnet with the WalletConnect wallet's actual chain before shipping the ceremony.
3. **Agent-wallet scope.** DOC-NONCE says API wallets "sign on behalf of the master account or any of the sub-accounts" and are "only used to sign"; no page read states explicitly which user-signed actions (withdraw3, usdSend, approveAgent) an API wallet is barred from. Spec D5's "no-withdraw scope" needs a citation or a testnet negative test.
4. **`PriorityGrouping {"p": int}`** exists in PY-SIGNING but not in DOC-EXCH. Do not use until documented.
5. **Units of `usd` in `subAccountTransfer`/`vaultTransfer`.** PY-TESTS uses `usd: 10`; DOC-EXCH says `"usd": number`; neither states the unit. (`ntli` for `updateIsolatedMargin` is documented as 6-decimal integer.)
6. **`agentName` on the wire for unnamed agents.** Python omits the key; rust sends `null`. Both sign `""`. Which the L1 prefers on the JSON side is unstated; omit (python behaviour) is the safer default.
7. **Multi-sig** (`sign_multi_sig_action`, `multiSig` envelope) is out of v1 scope and not covered here beyond the type list.
