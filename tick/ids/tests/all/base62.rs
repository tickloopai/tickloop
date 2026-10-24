use tickloop_ids::base62::{LENGTH, decode, decode_either, encode};
use uuid::Uuid;

const SHARED_VECTORS: &str = include_str!("../base62-vectors.json");

#[derive(serde::Deserialize)]
struct Vectors {
    vectors: Vec<Vector>,
}

#[derive(serde::Deserialize)]
struct Vector {
    uuid: Uuid,
    base62: String,
}

#[test]
fn matches_the_vectors_shared_with_cloud() {
    let vectors: Vectors = serde_json::from_str(SHARED_VECTORS).unwrap();
    assert!(!vectors.vectors.is_empty());
    for vector in vectors.vectors {
        assert_eq!(encode(vector.uuid), vector.base62, "{}", vector.uuid);
        assert_eq!(decode(&vector.base62), Some(vector.uuid));
    }
}

#[test]
fn round_trips_and_keeps_v7_time_order() {
    let ids: Vec<Uuid> = (0..200).map(|_| Uuid::now_v7()).collect();
    for pair in ids.windows(2) {
        assert!(encode(pair[0]) < encode(pair[1]));
    }
    for id in ids
        .into_iter()
        .chain([Uuid::nil(), Uuid::max(), Uuid::new_v4()])
    {
        let text = encode(id);
        assert_eq!(text.len(), LENGTH);
        assert_eq!(decode(&text), Some(id));
    }
}

#[test]
fn rejects_malformed_text() {
    let valid = encode(Uuid::now_v7());
    for text in [
        valid[1..].to_owned(),
        format!("{valid}0"),
        String::new(),
        format!("-{}", &valid[1..]),
        format!("{}é", &valid[..LENGTH - 2]),
        "z".repeat(LENGTH),
        "7n42DGM5Tflk9n8mt7Fhc8".to_owned(),
        Uuid::now_v7().to_string(),
    ] {
        assert_eq!(decode(&text), None, "{text}");
    }
}

#[test]
fn either_form_is_told_apart_by_length() {
    for id in [Uuid::nil(), Uuid::max(), Uuid::now_v7(), Uuid::new_v4()] {
        assert_eq!(decode_either(&encode(id)), Some(id));
        assert_eq!(decode_either(&id.to_string()), Some(id));
        assert_eq!(decode_either(&id.to_string().to_uppercase()), Some(id));
    }
    let id = Uuid::now_v7();
    for other in [
        id.simple().to_string(),
        id.urn().to_string(),
        id.braced().to_string(),
        encode(id).to_lowercase(),
        "not-an-id".to_owned(),
    ] {
        assert_ne!(decode_either(&other), Some(id), "{other}");
    }
}
