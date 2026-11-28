//! Minisign verification for release checksums and adapter indexes.
//!
//! The index is signed with minisign (Ed25519, prehashed mode). A signature made in the legacy
//! mode is refused: the library accepts it unless told not to. The trusted comment of the
//! signature binds what was signed (`index sequence=42 generated=2026-11-20T10:00:00Z`), and is
//! compared with the index body, so a valid old signature cannot be attached to a new body.
//!
//! The check is a small pure computation (no network, no file), so the CLI and the daemon both run
//! it: the daemon trusts the CLI for nothing it can check itself.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use minisign_verify::{PublicKey, Signature};

use crate::index::Index;

/// The current and next release signing keys, as base64 minisign public keys. The CLI and daemon
/// also use these keys for the official adapter index.
pub const OFFICIAL_INDEX_KEYS: &[&str] = &[
    "RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC",
    "RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5",
];

/// A public key the index may be signed with.
#[derive(Clone)]
pub struct IndexKey {
    /// The minisign key id, 16 upper-case hex digits.
    pub key_id: String,
    public: PublicKey,
    raw_id: [u8; 8],
}

impl std::fmt::Debug for IndexKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IndexKey({})", self.key_id)
    }
}

/// Who signed: the key id and the trusted comment that the signature covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedBy {
    pub key_id: String,
    pub trusted_comment: String,
}

/// Why a signature was not accepted. Every variant is `E_SIGNATURE_INVALID`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SigError {
    #[error("the signature file is not a minisign signature: {0}")]
    Malformed(String),
    #[error("the signature was made by key {0}, which is not one of the keys this build trusts")]
    UnknownKey(String),
    #[error("the signature does not match the signed data")]
    Invalid,
    #[error("the signature uses the legacy minisign mode, which is refused")]
    Legacy,
    #[error("the signature's trusted comment does not describe this index: {0}")]
    CommentMismatch(String),
    #[error("the bundle signature's trusted comment is {actual:?}, expected {expected:?}")]
    BundleCommentMismatch { actual: String, expected: String },
}

/// The key id as minisign prints it: the eight bytes in reverse order, upper-case hex.
fn key_id_text(raw: &[u8; 8]) -> String {
    raw.iter().rev().map(|b| format!("{b:02X}")).collect()
}

impl IndexKey {
    /// A minisign public key in base64 (the second line of a `minisign.pub` file; the whole file
    /// is accepted too).
    pub fn parse(text: &str) -> Result<IndexKey, SigError> {
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
            .ok_or_else(|| SigError::Malformed("an empty key".to_string()))?;
        let bytes = STANDARD
            .decode(line)
            .map_err(|e| SigError::Malformed(format!("the key is not base64: {e}")))?;
        if bytes.len() != 42 {
            return Err(SigError::Malformed(
                "a minisign public key is 42 bytes".to_string(),
            ));
        }
        let public =
            PublicKey::from_base64(line).map_err(|e| SigError::Malformed(format!("{e:?}")))?;
        let mut raw_id = [0u8; 8];
        raw_id.copy_from_slice(&bytes[2..10]);
        Ok(IndexKey {
            key_id: key_id_text(&raw_id),
            public,
            raw_id,
        })
    }
}

/// The keys embedded in this build.
pub fn official_keys() -> Vec<IndexKey> {
    OFFICIAL_INDEX_KEYS
        .iter()
        .filter_map(|k| IndexKey::parse(k).ok())
        .collect()
}

/// Whether the trust recorded after install verification identifies an official release.
pub fn is_official_release_trust(trust: &str) -> bool {
    trust == "bundled"
        || trust
            .strip_prefix("signed:")
            .is_some_and(|id| official_keys().iter().any(|key| key.key_id == id))
}

/// The key id and algorithm bytes of a signature text, without checking anything.
fn signature_header(sig_text: &str) -> Result<([u8; 2], [u8; 8]), SigError> {
    let line = sig_text
        .lines()
        .nth(1)
        .ok_or_else(|| SigError::Malformed("no signature line".to_string()))?;
    let bytes = STANDARD
        .decode(line.trim())
        .map_err(|e| SigError::Malformed(format!("the signature is not base64: {e}")))?;
    if bytes.len() != 74 {
        return Err(SigError::Malformed(
            "a minisign signature line is 74 bytes".to_string(),
        ));
    }
    let mut alg = [0u8; 2];
    alg.copy_from_slice(&bytes[0..2]);
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[2..10]);
    Ok((alg, id))
}

/// Check the minisign signature over `body` against `keys`. Only prehashed signatures from these
/// keys are accepted.
pub fn verify_minisign(
    body: &[u8],
    sig_text: &str,
    keys: &[IndexKey],
) -> Result<SignedBy, SigError> {
    let (alg, id) = signature_header(sig_text)?;
    if &alg == b"Ed" {
        return Err(SigError::Legacy);
    }
    let key = keys
        .iter()
        .find(|k| k.raw_id == id)
        .ok_or_else(|| SigError::UnknownKey(key_id_text(&id)))?;
    let signature =
        Signature::decode(sig_text).map_err(|e| SigError::Malformed(format!("{e:?}")))?;
    key.public
        .verify(body, &signature, false)
        .map_err(|e| match e {
            minisign_verify::Error::UnexpectedAlgorithm => SigError::Legacy,
            _ => SigError::Invalid,
        })?;
    Ok(SignedBy {
        key_id: key.key_id.clone(),
        trusted_comment: signature.trusted_comment().to_string(),
    })
}

/// The trusted comment for this release's adapter bundle on one target.
pub fn bundle_trusted_comment(tag: &str, target: &str) -> String {
    format!("oxidepm release bundle tag={tag} target={target}")
}

/// Verify the checksum list and its binding to the release tag and target.
pub fn verify_bundle_checksums(
    checksums: &[u8],
    sig_text: &str,
    tag: &str,
    target: &str,
    keys: &[IndexKey],
) -> Result<SignedBy, SigError> {
    let signed = verify_minisign(checksums, sig_text, keys)?;
    let expected = bundle_trusted_comment(tag, target);
    if signed.trusted_comment != expected {
        return Err(SigError::BundleCommentMismatch {
            actual: signed.trusted_comment,
            expected,
        });
    }
    Ok(signed)
}

/// The trusted comment a signature of this index must carry.
pub fn trusted_comment_for(index: &Index) -> String {
    format!(
        "index sequence={} generated={}",
        index.sequence,
        index.generated.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

/// The signature says what it signed: compare that with the index body.
pub fn check_trusted_comment(index: &Index, signed: &SignedBy) -> Result<(), SigError> {
    let want = trusted_comment_for(index);
    if signed.trusted_comment == want {
        Ok(())
    } else {
        Err(SigError::CommentMismatch(format!(
            "it says {:?}, the index is {:?}",
            signed.trusted_comment, want
        )))
    }
}

/// An index that was received: the bytes as they came, checked.
#[derive(Debug, Clone)]
pub struct VerifiedIndex {
    pub index: Index,
    pub signed_by: SignedBy,
}

/// Verify the signature over the bytes as received, parse them, and check the trusted comment
/// against the body. Time and the stored sequence number are not looked at here
/// ([`Index::check_fresh`] does that).
pub fn verify_index(
    body: &[u8],
    sig_text: &str,
    keys: &[IndexKey],
) -> Result<VerifiedIndex, crate::manifest::Invalid> {
    use crate::manifest::{invalid, Rule};
    let signed_by =
        verify_minisign(body, sig_text, keys).map_err(|e| crate::manifest::Invalid {
            rule: Rule::Signature,
            detail: e.to_string(),
        })?;
    let index = Index::parse(body)?;
    if let Err(e) = check_trusted_comment(&index, &signed_by) {
        return invalid(Rule::Signature, e.to_string());
    }
    Ok(VerifiedIndex { index, signed_by })
}

/// Throwaway signing keys for tests: they sign exactly as minisign does (prehashed `ED`, a trusted
/// comment covered by the global signature), and can also make the legacy `Ed` signature that must
/// be refused. Never part of a normal build.
#[cfg(feature = "test-support")]
pub mod testkeys {
    use super::*;
    use blake2::{Blake2b512, Digest};
    use ed25519_dalek::{Signer, SigningKey};

    pub struct TestKey {
        signing: SigningKey,
        id: [u8; 8],
    }

    fn random<const N: usize>() -> [u8; N] {
        use std::io::Read;
        let mut buf = [0u8; N];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .expect("read /dev/urandom");
        buf
    }

    impl TestKey {
        pub fn generate() -> TestKey {
            TestKey {
                signing: SigningKey::from_bytes(&random::<32>()),
                id: random::<8>(),
            }
        }

        /// The public key as `IndexKey::parse` and `OXIDEPM_TEST_INDEX_KEYS` take it.
        pub fn public_base64(&self) -> String {
            let mut bytes = Vec::with_capacity(42);
            bytes.extend_from_slice(b"Ed");
            bytes.extend_from_slice(&self.id);
            bytes.extend_from_slice(self.signing.verifying_key().as_bytes());
            STANDARD.encode(bytes)
        }

        pub fn key_id(&self) -> String {
            key_id_text(&self.id)
        }

        fn signature(&self, alg: &[u8; 2], message: &[u8], trusted_comment: &str) -> String {
            let sig = self.signing.sign(message).to_bytes();
            let mut line = Vec::with_capacity(74);
            line.extend_from_slice(alg);
            line.extend_from_slice(&self.id);
            line.extend_from_slice(&sig);
            let mut global_message = sig.to_vec();
            global_message.extend_from_slice(trusted_comment.as_bytes());
            let global = self.signing.sign(&global_message).to_bytes();
            format!(
                "untrusted comment: signature from a throwaway test key\n{}\ntrusted comment: {}\n{}\n",
                STANDARD.encode(line),
                trusted_comment,
                STANDARD.encode(global)
            )
        }

        /// A signature in the prehashed mode, as `minisign -S` makes.
        pub fn sign(&self, body: &[u8], trusted_comment: &str) -> String {
            let digest = Blake2b512::digest(body);
            self.signature(b"ED", &digest, trusted_comment)
        }

        /// A signature in the legacy mode (the raw message is signed).
        pub fn sign_legacy(&self, body: &[u8], trusted_comment: &str) -> String {
            self.signature(b"Ed", body, trusted_comment)
        }

        /// Sign an index body with the trusted comment its fields call for.
        pub fn sign_index(&self, body: &[u8]) -> String {
            let index = Index::parse(body).expect("a valid index to sign");
            self.sign(body, &trusted_comment_for(&index))
        }
    }
}
