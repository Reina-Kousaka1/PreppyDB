//! Logical log records: the unit of durability in the write-ahead log.
//!
//! A record is `{seq, ts, op}` serialised as canonical JSON. Keeping the payload
//! human-readable makes `preppydb-cli wal inspect` and post-mortem analysis possible;
//! the CRC-32C frame around it provides the actual corruption detection.

use crate::catalog::{CollectionMeta, IndexMeta};
use crate::error::{DbError, Result};
use preppydb_json::Json;

/// A single logical operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Put {
        collection: String,
        key: String,
        value: Json,
        expires_at: Option<i64>,
    },
    Delete {
        collection: String,
        key: String,
    },
    CreateCollection {
        meta: CollectionMeta,
    },
    DropCollection {
        collection: String,
    },
    CreateIndex {
        collection: String,
        index: IndexMeta,
    },
    DropIndex {
        collection: String,
        name: String,
    },
    /// Atomic group of operations. Either all of them are applied or none.
    Batch {
        ops: Vec<Op>,
        /// Human readable origin: `batch`, `transaction`, `ttl_expiry`, `import`.
        label: String,
    },
}

impl Op {
    pub fn to_json(&self) -> Json {
        match self {
            Op::Put {
                collection,
                key,
                value,
                expires_at,
            } => Json::obj()
                .with("op", Json::str("put"))
                .with("c", Json::str(collection.clone()))
                .with("k", Json::str(key.clone()))
                .with("v", value.clone())
                .with("e", expires_at.map(Json::int).unwrap_or(Json::Null)),
            Op::Delete { collection, key } => Json::obj()
                .with("op", Json::str("del"))
                .with("c", Json::str(collection.clone()))
                .with("k", Json::str(key.clone())),
            Op::CreateCollection { meta } => Json::obj()
                .with("op", Json::str("coll_create"))
                .with("meta", meta.to_json()),
            Op::DropCollection { collection } => Json::obj()
                .with("op", Json::str("coll_drop"))
                .with("c", Json::str(collection.clone())),
            Op::CreateIndex { collection, index } => Json::obj()
                .with("op", Json::str("index_create"))
                .with("c", Json::str(collection.clone()))
                .with("index", index.to_json()),
            Op::DropIndex { collection, name } => Json::obj()
                .with("op", Json::str("index_drop"))
                .with("c", Json::str(collection.clone()))
                .with("name", Json::str(name.clone())),
            Op::Batch { ops, label } => Json::obj()
                .with("op", Json::str("batch"))
                .with("label", Json::str(label.clone()))
                .with(
                    "ops",
                    Json::Array(ops.iter().map(|op| op.to_json()).collect()),
                ),
        }
    }

    pub fn from_json(value: &Json) -> Result<Op> {
        let kind = value
            .get_str("op")
            .ok_or_else(|| DbError::corrupt("record", "operation is missing 'op'"))?;
        let need_str = |key: &str| -> Result<String> {
            value
                .get_str(key)
                .map(|s| s.to_string())
                .ok_or_else(|| DbError::corrupt("record", format!("operation is missing '{key}'")))
        };
        match kind {
            "put" => {
                let expires_at = match value.get("e") {
                    None | Some(Json::Null) => None,
                    Some(other) => Some(other.as_i64().ok_or_else(|| {
                        DbError::corrupt("record", "expiry must be an integer or null")
                    })?),
                };
                Ok(Op::Put {
                    collection: need_str("c")?,
                    key: need_str("k")?,
                    value: value
                        .get("v")
                        .cloned()
                        .ok_or_else(|| DbError::corrupt("record", "put without 'v'"))?,
                    expires_at,
                })
            }
            "del" => Ok(Op::Delete {
                collection: need_str("c")?,
                key: need_str("k")?,
            }),
            "coll_create" => {
                let meta = value
                    .get("meta")
                    .ok_or_else(|| DbError::corrupt("record", "coll_create without 'meta'"))?;
                Ok(Op::CreateCollection {
                    meta: CollectionMeta::from_json(meta)?,
                })
            }
            "coll_drop" => Ok(Op::DropCollection {
                collection: need_str("c")?,
            }),
            "index_create" => {
                let index = value
                    .get("index")
                    .ok_or_else(|| DbError::corrupt("record", "index_create without 'index'"))?;
                Ok(Op::CreateIndex {
                    collection: need_str("c")?,
                    index: IndexMeta::from_json(index)?,
                })
            }
            "index_drop" => Ok(Op::DropIndex {
                collection: need_str("c")?,
                name: need_str("name")?,
            }),
            "batch" => {
                let ops = value
                    .get("ops")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| DbError::corrupt("record", "batch without 'ops'"))?;
                let mut decoded = Vec::with_capacity(ops.len());
                for entry in ops {
                    let op = Op::from_json(entry)?;
                    if matches!(op, Op::Batch { .. }) {
                        return Err(DbError::corrupt("record", "nested batches are not allowed"));
                    }
                    decoded.push(op);
                }
                Ok(Op::Batch {
                    ops: decoded,
                    label: value.get_str("label").unwrap_or("batch").to_string(),
                })
            }
            other => Err(DbError::corrupt(
                "record",
                format!("unknown operation '{other}'"),
            )),
        }
    }

    /// Collections touched by this operation (used for conflict reporting and stats).
    pub fn touched_collections(&self) -> Vec<&str> {
        match self {
            Op::Put { collection, .. }
            | Op::Delete { collection, .. }
            | Op::DropCollection { collection }
            | Op::CreateIndex { collection, .. }
            | Op::DropIndex { collection, .. } => vec![collection.as_str()],
            Op::CreateCollection { meta } => vec![meta.name.as_str()],
            Op::Batch { ops, .. } => {
                let mut names = Vec::new();
                for op in ops {
                    for name in op.touched_collections() {
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
                names
            }
        }
    }

    /// Number of individual document mutations contained in this operation.
    pub fn mutation_count(&self) -> usize {
        match self {
            Op::Put { .. } | Op::Delete { .. } => 1,
            Op::Batch { ops, .. } => ops.iter().map(|op| op.mutation_count()).sum(),
            _ => 0,
        }
    }
}

/// A log record with its sequence number and timestamp (milliseconds since epoch).
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub seq: u64,
    pub ts: i64,
    pub op: Op,
}

impl Record {
    pub fn new(seq: u64, ts: i64, op: Op) -> Record {
        Record { seq, ts, op }
    }

    pub fn to_json(&self) -> Json {
        Json::obj()
            .with("seq", Json::uint(self.seq))
            .with("ts", Json::int(self.ts))
            .with("op", self.op.to_json())
    }

    pub fn from_json(value: &Json) -> Result<Record> {
        let seq = value
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| DbError::corrupt("record", "record is missing 'seq'"))?;
        let ts = value
            .get("ts")
            .and_then(Json::as_i64)
            .ok_or_else(|| DbError::corrupt("record", "record has missing or invalid 'ts'"))?;
        let op_json = value
            .get("op")
            .ok_or_else(|| DbError::corrupt("record", "record is missing 'op'"))?;
        Ok(Record {
            seq,
            ts,
            op: Op::from_json(op_json)?,
        })
    }

    /// Encode the record body (without the WAL frame).
    pub fn encode_payload(&self) -> Vec<u8> {
        self.to_json().to_string().into_bytes()
    }

    pub fn decode_payload(payload: &[u8]) -> Result<Record> {
        let text = std::str::from_utf8(payload)
            .map_err(|err| DbError::corrupt("record", format!("payload is not UTF-8: {err}")))?;
        let value = preppydb_json::parse(text).map_err(|err| {
            DbError::corrupt("record", format!("payload is not valid JSON: {err}"))
        })?;
        Record::from_json(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_roundtrip() {
        let record = Record::new(
            7,
            1_700_000_000_000,
            Op::Put {
                collection: "profiles".into(),
                key: "molly".into(),
                value: Json::obj().with("name", Json::str("Molly")),
                expires_at: Some(42),
            },
        );
        let decoded = Record::decode_payload(&record.encode_payload()).unwrap();
        assert_eq!(record, decoded);
    }

    #[test]
    fn batch_roundtrip_and_mutation_count() {
        let batch = Op::Batch {
            label: "transaction".into(),
            ops: vec![
                Op::Put {
                    collection: "a".into(),
                    key: "1".into(),
                    value: Json::int(1),
                    expires_at: None,
                },
                Op::Delete {
                    collection: "a".into(),
                    key: "2".into(),
                },
            ],
        };
        assert_eq!(batch.mutation_count(), 2);
        let record = Record::new(1, 0, batch);
        let decoded = Record::decode_payload(&record.encode_payload()).unwrap();
        assert_eq!(record, decoded);
    }

    #[test]
    fn unknown_operation_is_corruption() {
        let json = preppydb_json::parse(r#"{"op":"teleport","c":"a"}"#).unwrap();
        assert!(Op::from_json(&json).is_err());
    }

    #[test]
    fn nested_batches_rejected() {
        let json =
            preppydb_json::parse(r#"{"op":"batch","ops":[{"op":"batch","ops":[]}]}"#).unwrap();
        assert!(Op::from_json(&json).is_err());
    }

    #[test]
    fn missing_or_malformed_record_fields_are_corruption() {
        for payload in [
            r#"{"seq":1,"op":{"op":"del","c":"c","k":"k"}}"#,
            r#"{"seq":1,"ts":"now","op":{"op":"del","c":"c","k":"k"}}"#,
            r#"{"seq":1,"ts":1,"op":{"op":"put","c":"c","k":"k"}}"#,
        ] {
            let error = Record::decode_payload(payload.as_bytes()).unwrap_err();
            assert_eq!(error.code(), crate::error::codes::CORRUPT_DATA);
        }
    }

    #[test]
    fn decoder_does_not_panic_on_truncations_or_deterministic_byte_mutations() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let payload = Record::new(
            17,
            1_700_000_000_000,
            Op::Put {
                collection: "fuzz_collection".into(),
                key: "fuzz_key".into(),
                value: Json::obj().with("value", Json::str("payload")),
                expires_at: None,
            },
        )
        .encode_payload();

        for end in 0..payload.len() {
            assert!(
                catch_unwind(|| Record::decode_payload(&payload[..end])).is_ok(),
                "decoder panicked on prefix length {end}"
            );
        }

        for index in 0..payload.len() {
            for replacement in [0, b'?', b'"', 0xff] {
                let mut mutated = payload.clone();
                mutated[index] = replacement;
                assert!(
                    catch_unwind(AssertUnwindSafe(|| Record::decode_payload(&mutated))).is_ok(),
                    "decoder panicked after mutating byte {index} to {replacement:#04x}"
                );
            }
        }
    }

    #[test]
    fn collection_ops_roundtrip() {
        let meta = CollectionMeta::new("shop_items", 5);
        let ops = vec![
            Op::CreateCollection { meta },
            Op::DropCollection {
                collection: "shop_items".into(),
            },
            Op::CreateIndex {
                collection: "shop_items".into(),
                index: IndexMeta {
                    name: "by_price".into(),
                    field: "price".into(),
                    unique: false,
                    sparse: true,
                    created_at: 1,
                },
            },
            Op::DropIndex {
                collection: "shop_items".into(),
                name: "by_price".into(),
            },
        ];
        for op in ops {
            let record = Record::new(3, 4, op);
            assert_eq!(
                record,
                Record::decode_payload(&record.encode_payload()).unwrap()
            );
        }
    }
}
