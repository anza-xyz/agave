#![cfg(feature = "agave-unstable-api")]
use solana_syscall_test_cases::{
    CASES, Error, INSTRUCTION_LEN, MAX_ITERATIONS, Request, by_id, by_name, execute,
};

#[test]
fn registry_has_unique_stable_ids_and_names() {
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for case in CASES {
        assert!(ids.insert(case.id));
        assert!(names.insert(case.name));
        assert_eq!(by_id(case.id).unwrap().name, case.name);
        assert_eq!(by_name(case.name).unwrap().id, case.id);
    }
    assert_eq!(by_name("keccak-1000-1").unwrap().id, 1);
    assert_eq!(by_name("sha256-1000").unwrap().id, 4);
    assert!(by_name("not-a-case").is_none());
    assert!(by_id(u32::MAX).is_none());
}

#[test]
fn all_enabled_cases_match_fixed_answers_and_repeat() {
    for case in CASES {
        let request = Request::new(case.name, 3, 42).unwrap();
        execute(&request.encode().unwrap())
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
    }
}

#[test]
fn wire_format_is_stable_and_nonce_only_changes_identity() {
    let request = Request::new("keccak-1000-1", 2, 9).unwrap();
    let bytes = request.encode().unwrap();
    assert_eq!(
        &bytes[..16],
        &[83, 67, 65, 83, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]
    );
    assert_eq!(Request::decode(&bytes), Ok(request));
    let other = Request {
        nonce: u64::MAX,
        ..request
    }
    .encode()
    .unwrap();
    assert_eq!(&bytes[..16], &other[..16]);
    assert_ne!(&bytes[16..], &other[16..]);
    execute(&other).unwrap();
}

#[test]
fn malformed_and_out_of_range_requests_are_rejected() {
    let data = Request::new("noop", 1, 0).unwrap().encode().unwrap();
    for length in 0..INSTRUCTION_LEN {
        assert_eq!(
            Request::decode(&data[..length]),
            Err(Error::InvalidInstruction)
        );
    }
    let mut trailing = data.to_vec();
    trailing.push(0);
    assert_eq!(Request::decode(&trailing), Err(Error::InvalidInstruction));
    for offset in [0, 4] {
        let mut bad = data;
        bad[offset] = 0xff;
        assert_eq!(Request::decode(&bad), Err(Error::InvalidInstruction));
    }
    for iterations in [0, MAX_ITERATIONS + 1, u32::MAX] {
        assert_eq!(
            Request::new("noop", iterations, 0),
            Err(Error::InvalidIterations)
        );
        let mut bad = data;
        bad[12..16].copy_from_slice(&iterations.to_le_bytes());
        assert_eq!(execute(&bad), Err(Error::InvalidIterations));
    }
    let mut unknown = data;
    unknown[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(execute(&unknown), Err(Error::UnknownCase));
}

#[cfg(not(feature = "modexp"))]
#[test]
fn disabled_family_is_not_advertised_or_executable() {
    assert!(by_name("modexp-128-e65537").is_none());
    let mut data = Request::new("noop", 1, 0).unwrap().encode().unwrap();
    data[8..12].copy_from_slice(&200u32.to_le_bytes());
    assert_eq!(execute(&data), Err(Error::UnknownCase));
}
