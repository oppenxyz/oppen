//! Replays `tests/vectors/signing.json` — 40 vectors copied verbatim from
//! the official python and rust SDK test suites (`docs/hl-signing.md`,
//! "Test vectors"). Every vector runs through the public API; L1 vectors
//! whose action is a supported [`Action`] also prove the typed structs
//! produce byte-identical msgpack to the source's dict/struct.

use serde::Deserialize;
use serde_json::Value;

use oppen_hl::signing::eip712::{FieldType, FieldValue, TypedData};
use oppen_hl::signing::user_signed::ApproveBuilderFee;
use oppen_hl::signing::{action_hash, keccak256};
use oppen_hl::{Action, Address, AgentKey, Network};

#[derive(Deserialize)]
struct Vector {
    name: String,
    network: String,
    kind: String,
    private_key: Option<String>,
    action: Option<Value>,
    connection_id: Option<String>,
    nonce: Option<u64>,
    vault_address: Option<String>,
    expires_after: Option<u64>,
    primary_type: Option<String>,
    eip712_types: Option<Vec<TypeEntry>>,
    expected: Expected,
}

#[derive(Deserialize)]
struct TypeEntry {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Deserialize, Default)]
struct Expected {
    r: Option<String>,
    s: Option<String>,
    v: Option<u8>,
    signature_hex: Option<String>,
    connection_id: Option<String>,
    action_hash: Option<String>,
}

const TYPED_ACTIONS: &[&str] = &[
    "order",
    "cancel",
    "cancelByCloid",
    "scheduleCancel",
    "updateLeverage",
    "updateIsolatedMargin",
    "createSubAccount",
    "subAccountTransfer",
    "claimRewards",
];

fn load() -> Vec<Vector> {
    let raw = include_str!("vectors/signing.json");
    serde_json::from_str(raw).expect("vectors parse")
}

/// Left-pads minimal hex (python `hex()`) to 32 bytes.
fn hex32(s: &str) -> [u8; 32] {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let padded = format!("{h:0>64}");
    let mut out = [0u8; 32];
    hex::decode_to_slice(&padded, &mut out).expect("hex32");
    out
}

fn network(s: &str) -> Network {
    match s {
        "mainnet" => Network::Mainnet,
        "testnet" => Network::Testnet,
        other => panic!("unknown network {other}"),
    }
}

fn check(name: &str, what: &str, ok: bool, failures: &mut Vec<String>) {
    if !ok {
        failures.push(format!("{name}: {what}"));
    }
}

fn run_l1(v: &Vector, failures: &mut Vec<String>) {
    let net = network(&v.network);

    if let Some(expected) = &v.expected.action_hash {
        // rs/test_approve_builder_fee_hash: alloy packs `builder` as 20 raw
        // bytes under rmp-serde's non-human-readable default. Pins the
        // msgpack ‖ nonce ‖ 0x00 ‖ keccak pipeline only.
        let action = v.action.as_ref().expect("action");
        let builder = Address::parse(action["builder"].as_str().unwrap()).unwrap();
        let mut buf = Vec::new();
        rmp::encode::write_map_len(&mut buf, 6).unwrap();
        for (k, val) in [
            ("type", action["type"].as_str().unwrap()),
            (
                "signatureChainId",
                action["signatureChainId"].as_str().unwrap(),
            ),
            (
                "hyperliquidChain",
                action["hyperliquidChain"].as_str().unwrap(),
            ),
        ] {
            rmp::encode::write_str(&mut buf, k).unwrap();
            rmp::encode::write_str(&mut buf, val).unwrap();
        }
        rmp::encode::write_str(&mut buf, "builder").unwrap();
        rmp::encode::write_bin(&mut buf, builder.as_bytes()).unwrap();
        rmp::encode::write_str(&mut buf, "maxFeeRate").unwrap();
        rmp::encode::write_str(&mut buf, action["maxFeeRate"].as_str().unwrap()).unwrap();
        rmp::encode::write_str(&mut buf, "nonce").unwrap();
        rmp::encode::write_uint(&mut buf, action["nonce"].as_u64().unwrap()).unwrap();
        let hash = action_hash(&buf, v.nonce.unwrap(), None, None);
        check(&v.name, "action_hash", hash == hex32(expected), failures);
        return;
    }

    let vault = v
        .vault_address
        .as_deref()
        .map(|a| Address::parse(a).unwrap());
    let connection_id = match (&v.action, &v.connection_id) {
        (Some(action), _) => {
            let msgpack = rmp_serde::to_vec_named(action).expect("msgpack value");
            let kind = action["type"].as_str().unwrap_or("");
            if TYPED_ACTIONS.contains(&kind) {
                match serde_json::from_value::<Action>(action.clone()) {
                    Ok(typed) => {
                        let typed_bytes = typed.to_msgpack().unwrap();
                        check(
                            &v.name,
                            "typed Action msgpack differs from source dict",
                            typed_bytes == msgpack,
                            failures,
                        );
                    }
                    // The rust SDK order vectors carry the un-normalized
                    // literal "2000.0"; `WireFloat` must refuse it
                    // (`docs/hl-signing.md` §4). They still pin the raw
                    // msgpack ‖ hash ‖ sign pipeline through the Value path.
                    Err(e) => {
                        let unnormalized = action.to_string().contains("\"2000.0\"");
                        check(
                            &v.name,
                            &format!("typed decode failed: {e}"),
                            unnormalized,
                            failures,
                        );
                    }
                }
            }
            action_hash(&msgpack, v.nonce.expect("nonce"), vault, v.expires_after)
        }
        (None, Some(cid)) => hex32(cid),
        (None, None) => panic!("{}: no action and no connection_id", v.name),
    };

    if let Some(expected) = &v.expected.connection_id {
        check(
            &v.name,
            "connection_id",
            connection_id == hex32(expected),
            failures,
        );
    }

    let Some(pk) = &v.private_key else { return };
    let key = AgentKey::from_hex(pk).unwrap();
    let sig = key.sign_connection_id(connection_id, net);
    check_signature(v, &sig, failures);
}

fn run_user(v: &Vector, failures: &mut Vec<String>) {
    let action = v.action.as_ref().expect("action");
    let chain_hex = action["signatureChainId"]
        .as_str()
        .expect("signatureChainId");
    let chain_id = u64::from_str_radix(chain_hex.trim_start_matches("0x"), 16).unwrap();
    let fields = v
        .eip712_types
        .as_ref()
        .expect("eip712_types")
        .iter()
        .map(|t| {
            let raw = &action[&t.name];
            let value =
                match FieldType::parse(&t.ty).unwrap_or_else(|| panic!("unknown type {}", t.ty)) {
                    FieldType::String => FieldValue::String(raw.as_str().unwrap().to_owned()),
                    FieldType::Address => {
                        FieldValue::Address(Address::parse(raw.as_str().unwrap()).unwrap())
                    }
                    FieldType::Uint64 => FieldValue::Uint64(raw.as_u64().unwrap()),
                    FieldType::Bool => FieldValue::Bool(raw.as_bool().unwrap()),
                    FieldType::Bytes32 => FieldValue::Bytes32(hex32(raw.as_str().unwrap())),
                };
            (t.name.clone(), value)
        })
        .collect();
    let td = TypedData {
        domain_name: "HyperliquidSignTransaction".into(),
        chain_id,
        primary_type: v.primary_type.clone().expect("primary_type"),
        fields,
    };
    let digest = td.signing_hash();

    if action["type"] == "approveBuilderFee" {
        let net = match action["hyperliquidChain"].as_str().unwrap() {
            "Mainnet" => Network::Mainnet,
            _ => Network::Testnet,
        };
        let helper = ApproveBuilderFee::new(
            net,
            chain_id,
            action["maxFeeRate"].as_str().unwrap().to_owned(),
            Address::parse(action["builder"].as_str().unwrap()).unwrap(),
            action["nonce"].as_u64().unwrap(),
        );
        check(
            &v.name,
            "ApproveBuilderFee helper digest",
            helper.typed_data().signing_hash() == digest,
            failures,
        );
    }

    let key = AgentKey::from_hex(v.private_key.as_deref().expect("private_key")).unwrap();
    let sig = key.sign_digest(&digest);
    check_signature(v, &sig, failures);
}

fn check_signature(v: &Vector, sig: &oppen_hl::Signature, failures: &mut Vec<String>) {
    if let Some(hex) = &v.expected.signature_hex {
        check(
            &v.name,
            &format!("signature_hex got {}", sig.to_hex()),
            sig.to_hex() == *hex,
            failures,
        );
    }
    if let (Some(r), Some(s), Some(vv)) = (&v.expected.r, &v.expected.s, v.expected.v) {
        check(&v.name, "r", sig.r == hex32(r), failures);
        check(&v.name, "s", sig.s == hex32(s), failures);
        check(&v.name, "v", sig.v == vv, failures);
    }
}

#[test]
fn all_official_vectors_pass() {
    let vectors = load();
    assert_eq!(vectors.len(), 40, "vector count");
    let mut failures = Vec::new();
    for v in &vectors {
        match v.kind.as_str() {
            "l1" => run_l1(v, &mut failures),
            "user" => run_user(v, &mut failures),
            other => panic!("{}: unknown kind {other}", v.name),
        }
    }
    assert!(
        failures.is_empty(),
        "failing vectors:\n{}",
        failures.join("\n")
    );
}

#[test]
fn keccak_is_the_ethereum_variant() {
    assert_eq!(
        hex::encode(keccak256(b"")),
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
    );
}
