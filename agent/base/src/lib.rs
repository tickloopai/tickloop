//! Leaf utilities shared by every Agent crate: private files, credentials, timestamps, HTTP policy, images and
//! output styles.

pub mod credentials;
pub mod image;
pub mod network;
pub mod output_style;
pub mod private_fs;
pub mod timestamp;

/// Stable bytes for JSON identities, independent of serde_json's unified `preserve_order` feature.
/// Object keys are sorted recursively; array order remains significant.
pub fn canonical_json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut value = value.clone();
    value.sort_all_objects();
    serde_json::to_vec(&value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_identity_is_independent_of_object_insertion_order() {
        let first: serde_json::Value =
            serde_json::from_str(r#"{"z":[{"b":2,"a":1}],"a":0}"#).unwrap();
        let second: serde_json::Value =
            serde_json::from_str(r#"{"a":0,"z":[{"a":1,"b":2}]}"#).unwrap();
        assert_eq!(
            super::canonical_json_bytes(&first).unwrap(),
            br#"{"a":0,"z":[{"a":1,"b":2}]}"#
        );
        assert_eq!(
            super::canonical_json_bytes(&first).unwrap(),
            super::canonical_json_bytes(&second).unwrap()
        );
    }
}
