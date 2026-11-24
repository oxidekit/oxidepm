//! Error codes. The stable names (`E_...`) travel in `error.data.code`; the numeric JSON-RPC codes
//! are the ones reserved by JSON-RPC and OxidePM's own range `-32020` to `-32099`.

/// JSON-RPC numeric codes.
pub mod rpc {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const UNSUPPORTED_PROTOCOL: i64 = -32022;
    pub const PERMISSION_DENIED: i64 = -32023;
    pub const CANCELLED: i64 = -32024;
    pub const TIMEOUT: i64 = -32025;
    pub const SHUTTING_DOWN: i64 = -32026;
    pub const DRY_RUN_UNSUPPORTED: i64 = -32027;
    pub const RATE_LIMITED: i64 = -32028;
    pub const BUSY: i64 = -32029;
}

pub const E_ADAPTER_MISSING: &str = "E_ADAPTER_MISSING";
pub const E_ADAPTER_UNAVAILABLE: &str = "E_ADAPTER_UNAVAILABLE";
pub const E_ADAPTER_TIMEOUT: &str = "E_ADAPTER_TIMEOUT";
pub const E_ADAPTER_CRASHED: &str = "E_ADAPTER_CRASHED";
pub const E_ADAPTER_PROTOCOL: &str = "E_ADAPTER_PROTOCOL";
pub const E_ADAPTER_BUSY: &str = "E_ADAPTER_BUSY";
pub const E_ADAPTER_INVALID: &str = "E_ADAPTER_INVALID";
pub const E_ADAPTER_TAMPERED: &str = "E_ADAPTER_TAMPERED";
pub const E_POLICY_DENIED: &str = "E_POLICY_DENIED";
pub const E_RATE_LIMITED: &str = "E_RATE_LIMITED";
pub const E_REQUEST_TOO_LARGE: &str = "E_REQUEST_TOO_LARGE";
pub const E_DRY_RUN_UNSUPPORTED: &str = "E_DRY_RUN_UNSUPPORTED";
pub const E_CHECKSUM_MISMATCH: &str = "E_CHECKSUM_MISMATCH";
pub const E_SIGNATURE_INVALID: &str = "E_SIGNATURE_INVALID";
pub const E_INDEX_STALE: &str = "E_INDEX_STALE";

/// Every stable name above, for tests and for code that must recognise one.
pub const ALL_NAMES: &[&str] = &[
    E_ADAPTER_MISSING,
    E_ADAPTER_UNAVAILABLE,
    E_ADAPTER_TIMEOUT,
    E_ADAPTER_CRASHED,
    E_ADAPTER_PROTOCOL,
    E_ADAPTER_BUSY,
    E_ADAPTER_INVALID,
    E_ADAPTER_TAMPERED,
    E_POLICY_DENIED,
    E_RATE_LIMITED,
    E_REQUEST_TOO_LARGE,
    E_DRY_RUN_UNSUPPORTED,
    E_CHECKSUM_MISMATCH,
    E_SIGNATURE_INVALID,
    E_INDEX_STALE,
];

/// The stable name that goes with a numeric code of OxidePM's own range, where there is one.
pub fn name_for_rpc_code(code: i64) -> Option<&'static str> {
    Some(match code {
        rpc::UNSUPPORTED_PROTOCOL => E_ADAPTER_PROTOCOL,
        rpc::PERMISSION_DENIED => E_POLICY_DENIED,
        rpc::TIMEOUT => E_ADAPTER_TIMEOUT,
        rpc::DRY_RUN_UNSUPPORTED => E_DRY_RUN_UNSUPPORTED,
        rpc::RATE_LIMITED => E_RATE_LIMITED,
        rpc::BUSY => E_ADAPTER_BUSY,
        _ => return None,
    })
}
