use serde_json::{json, Value};

use crate::codes;
use crate::manifest::Rule;
use crate::signing::testkeys::TestKey;
use crate::signing::{
    bundle_trusted_comment, is_official_release_trust, official_keys, trusted_comment_for,
    verify_bundle_checksums, verify_index, verify_minisign, IndexKey, SigError,
};
use crate::Index;

fn index_json(sequence: u64) -> Value {
    json!({
        "schema": 1, "sequence": sequence,
        "generated": "2026-11-20T10:00:00Z", "expires": "2027-01-20T10:00:00Z",
        "revoked": [], "adapters": []
    })
}

fn body(sequence: u64) -> Vec<u8> {
    index_json(sequence).to_string().into_bytes()
}

fn key_of(k: &TestKey) -> IndexKey {
    IndexKey::parse(&k.public_base64()).unwrap()
}

#[test]
fn bundle_signature_covers_checksums_tag_and_target() {
    let signer = TestKey::generate();
    let checksums = b"checksums as shipped\n";
    let tag = "v1.2.3-rc.1";
    let target = "x86_64-unknown-linux-gnu";
    let comment = bundle_trusted_comment(tag, target);
    assert_eq!(
        comment,
        "oxidepm release bundle tag=v1.2.3-rc.1 target=x86_64-unknown-linux-gnu"
    );
    let keys = [key_of(&signer)];
    let sig = signer.sign(checksums, &comment);

    let signed = verify_bundle_checksums(checksums, &sig, tag, target, &keys).unwrap();
    assert_eq!(signed.key_id, signer.key_id());
    assert_eq!(signed.trusted_comment, comment);
    assert_eq!(
        verify_bundle_checksums(b"changed checksums\n", &sig, tag, target, &keys),
        Err(SigError::Invalid)
    );
    for (other_tag, other_target) in [("v1.2.3-rc.2", target), (tag, "aarch64-unknown-linux-gnu")] {
        let expected = bundle_trusted_comment(other_tag, other_target);
        assert_eq!(
            verify_bundle_checksums(checksums, &sig, other_tag, other_target, &keys),
            Err(SigError::BundleCommentMismatch {
                actual: comment.clone(),
                expected,
            })
        );
    }
}

#[test]
fn bundle_signature_requires_a_trusted_key_and_prehashed_mode() {
    let signer = TestKey::generate();
    let other = TestKey::generate();
    let checksums = b"bundle checksums\n";
    let tag = "v1.2.3";
    let target = "aarch64-unknown-linux-gnu";
    let comment = bundle_trusted_comment(tag, target);
    assert_eq!(
        verify_bundle_checksums(
            checksums,
            &other.sign(checksums, &comment),
            tag,
            target,
            &[key_of(&signer)],
        ),
        Err(SigError::UnknownKey(other.key_id()))
    );
    assert_eq!(
        verify_bundle_checksums(
            checksums,
            &signer.sign_legacy(checksums, &comment),
            tag,
            target,
            &[key_of(&signer)],
        ),
        Err(SigError::Legacy)
    );
}

#[test]
fn official_release_trust_requires_a_verified_bundle_or_embedded_key() {
    assert!(is_official_release_trust("bundled:verified"));
    assert!(!is_official_release_trust("bundled"));
    for key in official_keys() {
        assert!(is_official_release_trust(&format!("signed:{}", key.key_id)));
    }
    assert!(!is_official_release_trust("pinned"));
    let other = TestKey::generate();
    assert!(!is_official_release_trust(&format!(
        "signed:{}",
        other.key_id()
    )));
}

#[test]
fn a_signature_by_a_listed_key_verifies_and_names_the_key() {
    let k = TestKey::generate();
    let b = body(42);
    let sig = k.sign_index(&b);
    let signed = verify_minisign(&b, &sig, &[key_of(&k)]).unwrap();
    assert_eq!(signed.key_id, k.key_id());
    assert_eq!(
        signed.trusted_comment,
        "index sequence=42 generated=2026-11-20T10:00:00Z"
    );
    let verified = verify_index(&b, &sig, &[key_of(&k)]).unwrap();
    assert_eq!(verified.index.sequence, 42);
    assert_eq!(verified.signed_by.key_id, k.key_id());
}

#[test]
fn a_signature_made_by_the_minisign_crate_verifies() {
    // the reference implementation, not our own signer
    let (pk, sk) = {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        (pair.pk, pair.sk)
    };
    let b = body(7);
    let index = Index::parse(&b).unwrap();
    let boxed = minisign::sign(
        Some(&pk),
        &sk,
        std::io::Cursor::new(&b),
        Some(&trusted_comment_for(&index)),
        Some("test"),
    )
    .unwrap();
    let key = IndexKey::parse(&pk.to_box().unwrap().to_string()).unwrap();
    let signed = verify_minisign(&b, &boxed.to_string(), std::slice::from_ref(&key)).unwrap();
    assert_eq!(signed.key_id, key.key_id);
    verify_index(&b, &boxed.to_string(), &[key]).unwrap();
}

#[test]
fn a_tampered_body_is_refused() {
    let k = TestKey::generate();
    let sig = k.sign_index(&body(42));
    let mut other = body(42);
    other.extend_from_slice(b" ");
    let error = verify_minisign(&other, &sig, &[key_of(&k)]).unwrap_err();
    assert_eq!(error, SigError::Invalid);
    assert_eq!(
        error.to_string(),
        "the signature does not match the signed data"
    );
}

#[test]
fn an_unknown_key_is_refused_there_is_no_trust_on_first_use() {
    let signer = TestKey::generate();
    let other = TestKey::generate();
    let b = body(42);
    let sig = signer.sign_index(&b);
    assert_eq!(
        verify_minisign(&b, &sig, &[key_of(&other)]),
        Err(SigError::UnknownKey(signer.key_id()))
    );
    assert_eq!(
        verify_minisign(&b, &sig, &[]),
        Err(SigError::UnknownKey(signer.key_id()))
    );
    let err = verify_index(&b, &sig, &[key_of(&other)]).unwrap_err();
    assert_eq!(err.rule, Rule::Signature);
    assert_eq!(err.rule.error_code(), codes::E_SIGNATURE_INVALID);
}

#[test]
fn a_legacy_signature_is_refused_although_the_library_would_accept_it() {
    let k = TestKey::generate();
    let b = body(42);
    let sig = k.sign_legacy(&b, &trusted_comment_for(&Index::parse(&b).unwrap()));
    // the library accepts the legacy mode when it is told to
    let key = minisign_verify::PublicKey::from_base64(&k.public_base64()).unwrap();
    let decoded = minisign_verify::Signature::decode(&sig).unwrap();
    key.verify(&b, &decoded, true)
        .expect("a legacy signature is a valid signature");
    // and we do not tell it to
    assert_eq!(
        verify_minisign(&b, &sig, &[key_of(&k)]),
        Err(SigError::Legacy)
    );
}

#[test]
fn an_old_signature_cannot_be_attached_to_a_new_body() {
    let k = TestKey::generate();
    // the signature of sequence 41 over the body of sequence 41 ...
    let old_body = body(41);
    let old_sig = k.sign_index(&old_body);
    // ... is valid for that body only
    verify_index(&old_body, &old_sig, &[key_of(&k)]).unwrap();
    // a signature whose trusted comment says 41 over the body of 42 is refused (the signature
    // itself is valid: the signer signed that body under that comment)
    let new_body = body(42);
    let mismatched = k.sign(
        &new_body,
        "index sequence=41 generated=2026-11-20T10:00:00Z",
    );
    verify_minisign(&new_body, &mismatched, &[key_of(&k)]).unwrap();
    let err = verify_index(&new_body, &mismatched, &[key_of(&k)]).unwrap_err();
    assert_eq!(err.rule, Rule::Signature);
    assert!(err.detail.contains("trusted comment"), "{}", err.detail);
}

#[test]
fn a_malformed_key_or_signature_is_refused() {
    assert!(IndexKey::parse("").is_err());
    assert!(IndexKey::parse("not base64 !!").is_err());
    assert!(IndexKey::parse("AAAA").is_err());
    let k = TestKey::generate();
    assert!(matches!(
        verify_minisign(b"x", "", &[key_of(&k)]),
        Err(SigError::Malformed(_))
    ));
    assert!(matches!(
        verify_minisign(b"x", "one\ntwo", &[key_of(&k)]),
        Err(SigError::Malformed(_))
    ));
}

#[test]
fn the_key_file_of_minisign_is_accepted_whole() {
    let k = TestKey::generate();
    let file = format!(
        "untrusted comment: minisign public key\n{}\n",
        k.public_base64()
    );
    assert_eq!(IndexKey::parse(&file).unwrap().key_id, k.key_id());
}

#[test]
fn both_official_signing_keys_are_embedded() {
    assert_eq!(
        crate::signing::OFFICIAL_INDEX_KEYS,
        [
            "RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC",
            "RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5",
        ],
    );
    let keys = crate::official_keys();
    let ids: Vec<_> = keys.iter().map(|key| key.key_id.as_str()).collect();
    assert_eq!(ids, ["04BF7D2A1951790C", "63BC416033EA24EE"]);
}

#[test]
fn http_urls_are_accepted_only_for_the_loopback_in_test_builds() {
    let mut v = index_json(1);
    v["adapters"] = json!([{
        "name": "demo", "kind": "tool", "summary": "x", "releases": [{
            "version": "0.1.0", "released": "2026-11-20", "min_daemon": "0.1.0", "protocol": [1],
            "artifacts": {"x86_64-unknown-linux-gnu": {"url": "http://127.0.0.1:9/demo.tar.gz", "size": 10,
                           "sha256": "a".repeat(64), "exe_sha256": "b".repeat(64)}}
        }]
    }]);
    Index::parse(v.to_string().as_bytes()).expect("loopback http in a test build");
    v["adapters"][0]["releases"][0]["artifacts"]["x86_64-unknown-linux-gnu"]["url"] =
        json!("http://example.com/demo.tar.gz");
    assert!(Index::parse(v.to_string().as_bytes()).is_err());
    v["adapters"][0]["releases"][0]["artifacts"]["x86_64-unknown-linux-gnu"]["url"] =
        json!("http://127.0.0.1.evil.com/demo.tar.gz");
    assert!(Index::parse(v.to_string().as_bytes()).is_err());
}
