use num_bigint::BigUint;
use trindrome_core::{CoreError, SearchSpace, WorkChunk};

fn big(v: u64) -> BigUint {
    BigUint::from(v)
}

fn frontier_a() -> WorkChunk {
    WorkChunk::whole(SearchSpace::new(65, 0x1).unwrap())
}

const FRONTIER_A_JSON: &str = r#"{"schema_version":1,"generator_version":1,"hex_digits":65,"leading_hex_digit":1,"start_index":"0","end_index":"4294967296"}"#;

#[test]
fn serialization_is_byte_stable() {
    assert_eq!(serde_json::to_string(&frontier_a()).unwrap(), FRONTIER_A_JSON);
    let parsed: WorkChunk = serde_json::from_str(FRONTIER_A_JSON).unwrap();
    assert_eq!(parsed, frontier_a());
    parsed.validate().unwrap();
}

#[test]
fn indices_beyond_javascript_precision_round_trip() {
    let s = SearchSpace::new(200, 0x9).unwrap();
    let start = BigUint::parse_bytes(b"123456789012345678901234567890", 10).unwrap();
    let chunk = WorkChunk::new(s, start.clone(), start + 1u32).unwrap();
    let json = serde_json::to_string(&chunk).unwrap();
    assert!(json.contains(r#""start_index":"123456789012345678901234567890""#));
    assert_eq!(serde_json::from_str::<WorkChunk>(&json).unwrap(), chunk);
}

#[test]
fn non_canonical_indices_are_rejected() {
    for bad in [r#""01""#, r#""-1""#, r#""1e3""#, r#""""#, r#"" 1""#, "7"] {
        let json = FRONTIER_A_JSON.replace(r#""0""#, bad);
        assert!(serde_json::from_str::<WorkChunk>(&json).is_err(), "{bad} accepted");
    }
    let extra = FRONTIER_A_JSON.replace('}', r#","backend":"cuda"}"#);
    assert!(serde_json::from_str::<WorkChunk>(&extra).is_err());
}

#[test]
fn identity_depends_only_on_the_mathematics() {
    assert_eq!(frontier_a().id(), "trindrome:g1:h65:1:0-4294967296");
    let f = WorkChunk::whole(SearchSpace::new(33, 0xF).unwrap());
    assert_eq!(f.id(), "trindrome:g1:h33:F:0-4294967296");
    assert_ne!(f.id(), frontier_a().id());
}

#[test]
fn validation_rejects_bad_chunks() {
    let good = frontier_a();
    let mutate = |f: &dyn Fn(&mut WorkChunk)| {
        let mut c = good.clone();
        f(&mut c);
        c.validate()
    };
    assert_eq!(
        mutate(&|c| c.schema_version = 2),
        Err(CoreError::UnsupportedSchemaVersion(2))
    );
    assert_eq!(
        mutate(&|c| c.generator_version = 0),
        Err(CoreError::UnsupportedGeneratorVersion(0))
    );
    assert_eq!(
        mutate(&|c| c.leading_hex_digit = 0xA),
        Err(CoreError::InvalidLeadingDigit(0xA))
    );
    assert_eq!(mutate(&|c| c.hex_digits = 0), Err(CoreError::InvalidHexDigits(0)));
    assert!(matches!(
        mutate(&|c| c.end_index = big(0)),
        Err(CoreError::InvalidRange { .. })
    ));
    assert!(matches!(
        mutate(&|c| c.end_index += 1u32),
        Err(CoreError::InvalidRange { .. })
    ));
}

#[test]
fn splitting_covers_the_range_exactly() {
    let whole = WorkChunk::whole(SearchSpace::new(9, 0x5).unwrap()); // 256 candidates
    for max in [1u64, 7, 100, 256, 1000] {
        let parts = whole.split(&big(max));
        assert_eq!(parts.first().unwrap().start_index, whole.start_index);
        assert_eq!(parts.last().unwrap().end_index, whole.end_index);
        assert!(parts.windows(2).all(|w| w[0].is_followed_by(&w[1])));
        assert!(parts.iter().all(|p| p.validate().is_ok() && p.len() <= big(max)));
        assert_eq!(parts.iter().map(WorkChunk::len).sum::<BigUint>(), whole.len());
        assert_eq!(parts.len() as u64, 256u64.div_ceil(max));

        let mut ids: Vec<String> = parts.iter().map(WorkChunk::id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), parts.len());
    }
}
