//! The `$JSON` metadata block.
//!
//! The payload is byte-exactly what `nlohmann::json::dump(4)` produces with sorted keys,
//! plus a trailing newline. Confirmed on every file of the test corpus by
//! `scratch/probe.py`. That lets this crate parse into a generic value, patch a few paths,
//! and re-emit, instead of doing text surgery on the raw bytes.
//!
//! Two rules keep that safe. The crate never models the document as a Rust struct, because
//! Macrium writes keys the published schema does not list and a typed round-trip would
//! drop them. And [`check_round_trip`] re-serializes the untouched value and compares it
//! against the source before anything is patched, so a future Reflect version that emits a
//! shape this serializer cannot reproduce fails loudly rather than quietly.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

/// Parse a `$JSON` payload.
pub fn parse(raw: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(raw).context("parsing the $JSON block")?;
    if !value.is_object() {
        bail!("the $JSON block is not a JSON object");
    }
    Ok(value)
}

/// Serialize in the canonical form the format uses.
///
/// `serde_json` stores object keys in a `BTreeMap`, which sorts them, so this matches
/// `dump(4)` with `sort_keys` as long as the `preserve_order` feature stays off.
pub fn canonical(value: &Value) -> Result<Vec<u8>> {
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(value, &mut ser).context("serializing the $JSON block")?;
    out.push(b'\n');
    Ok(out)
}

/// Make sure that re-serializing `value` reproduces `original` byte for byte.
///
/// Call this before patching. A mismatch means this crate cannot safely rewrite the
/// metadata of this file, and the only honest response is to refuse.
pub fn check_round_trip(value: &Value, original: &[u8]) -> Result<()> {
    let again = canonical(value)?;
    if again != original {
        bail!(
            "JSON canonical form mismatch; refusing to rewrite metadata \
             (source is {} bytes, re-serialized is {} bytes)",
            original.len(),
            again.len()
        );
    }
    Ok(())
}

/// The `_header` fields this crate reads. Everything else in the document is carried
/// through untouched.
#[derive(Debug, Clone)]
pub struct Header {
    /// Backup set identity: 16 hex characters, that is 8 bytes.
    pub imageid: String,
    pub file_number: u16,
    pub increment_number: u16,
    /// File numbers this file absorbed through a previous consolidation. Absent in every
    /// unconsolidated file, so it reads as an empty list.
    pub merged_files: Vec<i64>,
    pub split_file: bool,
    pub index_file_position: u64,
    /// Decides the form of the `$INDEX` block arrays.
    pub delta_index: bool,
    /// `full`, `inc` or `diff`. Records the backup definition's type, not the file's role
    /// in the chain, so never use it to find the Full.
    pub backup_type: String,
}

impl Header {
    pub fn from_value(value: &Value) -> Result<Self> {
        let h = value
            .get("_header")
            .and_then(Value::as_object)
            .context("the $JSON block has no _header object")?;

        Ok(Self {
            imageid: string(h, "imageid")?,
            file_number: u16::try_from(u64_field(h, "file_number")?)
                .context("file_number does not fit in 16 bits")?,
            increment_number: u16::try_from(u64_field(h, "increment_number")?)
                .context("increment_number does not fit in 16 bits")?,
            merged_files: merged_files(h)?,
            split_file: h
                .get("split_file")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            index_file_position: u64_field(h, "index_file_position")?,
            // The reference struct defaults this to true, so an absent key means delta.
            delta_index: h
                .get("delta_index")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            backup_type: h
                .get("backup_type")
                .and_then(Value::as_str)
                .unwrap_or("full")
                .to_string(),
        })
    }

    /// True when this file carries a complete index of its own, which is what identifies
    /// the Full of a chain. A split continuation carries no index at all.
    pub fn is_full_index(&self) -> bool {
        !self.delta_index && !self.split_file
    }

    pub fn is_differential(&self) -> bool {
        self.backup_type == "diff"
    }

    /// Every file number whose blocks resolve to this file: its own, plus everything it
    /// absorbed.
    pub fn owned_file_numbers(&self) -> Vec<u16> {
        let mut out = vec![self.file_number];
        for n in &self.merged_files {
            if let Ok(n) = u16::try_from(*n) {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    }

    /// The 8 raw bytes behind `imageid`. These are the encryption salt input and the first
    /// eight bytes of every per-block initialization vector.
    pub fn imageid_binary(&self) -> Result<[u8; 8]> {
        let s = &self.imageid;
        if s.len() != 16 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("imageid {s:?} is not 16 hex characters");
        }
        let mut out = [0u8; 8];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("checked hex above");
        }
        Ok(out)
    }
}

fn string(map: &Map<String, Value>, key: &str) -> Result<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("_header.{key} is missing or is not a string"))
}

fn u64_field(map: &Map<String, Value>, key: &str) -> Result<u64> {
    map.get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("_header.{key} is missing or is not a non-negative integer"))
}

fn merged_files(map: &Map<String, Value>) -> Result<Vec<i64>> {
    let Some(value) = map.get("merged_files") else {
        return Ok(Vec::new());
    };
    let arr = value
        .as_array()
        .context("_header.merged_files is not an array")?;
    arr.iter()
        .map(|v| {
            v.as_i64()
                .context("_header.merged_files holds a non-integer entry")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "_compression": { "compression_level": "none", "compression_method": "zstd" },
            "_encryption": { "enable": false, "key_iterations": 0 },
            "_header": {
                "backup_type": "full",
                "delta_index": false,
                "file_number": 0,
                "imageid": "DD5A77E6B68A6C34",
                "increment_number": 0,
                "index_file_position": 13041664u64,
                "split_file": false
            },
            "disks": []
        })
    }

    #[test]
    fn canonical_uses_four_spaces_sorted_keys_and_a_trailing_newline() {
        let bytes = canonical(&doc()).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with("}\n"), "must end with a newline");
        assert!(text.contains("\n    \"_compression\""), "four-space indent");
        // Keys sort, so _compression precedes _encryption precedes _header precedes disks.
        let pos = |k: &str| text.find(k).unwrap();
        assert!(pos("\"_compression\"") < pos("\"_encryption\""));
        assert!(pos("\"_encryption\"") < pos("\"_header\""));
        assert!(pos("\"_header\"") < pos("\"disks\""));
    }

    #[test]
    fn empty_containers_render_the_way_nlohmann_renders_them() {
        let bytes = canonical(&json!({ "a": {}, "b": [] })).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\n    \"a\": {},\n    \"b\": []\n}\n"
        );
    }

    #[test]
    fn the_round_trip_gate_accepts_its_own_output() {
        let original = canonical(&doc()).unwrap();
        let parsed = parse(&original).unwrap();
        check_round_trip(&parsed, &original).unwrap();
    }

    #[test]
    fn the_round_trip_gate_rejects_a_shape_it_cannot_reproduce() {
        // Two-space indent is a stand-in for any future formatting change.
        let original = serde_json::to_vec_pretty(&doc()).unwrap();
        let parsed = parse(&original).unwrap();
        let err = check_round_trip(&parsed, &original).unwrap_err();
        assert!(
            err.to_string().contains("refusing to rewrite metadata"),
            "{err}"
        );
    }

    #[test]
    fn header_reads_the_fields_the_tool_needs() {
        let h = Header::from_value(&doc()).unwrap();
        assert_eq!(h.imageid, "DD5A77E6B68A6C34");
        assert_eq!(h.file_number, 0);
        assert!(h.is_full_index());
        assert!(!h.is_differential());
        assert_eq!(h.merged_files, Vec::<i64>::new());
    }

    #[test]
    fn an_absent_delta_index_key_defaults_to_delta() {
        // The reference struct initializes delta_index to true.
        let mut v = doc();
        v["_header"].as_object_mut().unwrap().remove("delta_index");
        assert!(Header::from_value(&v).unwrap().delta_index);
    }

    #[test]
    fn owned_file_numbers_cover_the_merge_aliases() {
        let mut v = doc();
        v["_header"]["file_number"] = json!(5);
        v["_header"]["merged_files"] = json!([3, 4]);
        let h = Header::from_value(&v).unwrap();
        assert_eq!(h.owned_file_numbers(), vec![5, 3, 4]);
    }

    #[test]
    fn imageid_decodes_to_eight_bytes() {
        let h = Header::from_value(&doc()).unwrap();
        assert_eq!(
            h.imageid_binary().unwrap(),
            [0xDD, 0x5A, 0x77, 0xE6, 0xB6, 0x8A, 0x6C, 0x34]
        );
    }

    #[test]
    fn a_malformed_imageid_is_rejected() {
        let mut v = doc();
        v["_header"]["imageid"] = json!("not hex at all!");
        let h = Header::from_value(&v).unwrap();
        assert!(h.imageid_binary().is_err());
    }
}
