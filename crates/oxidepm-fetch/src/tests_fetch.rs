use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::testserver::{serve, Reply};
use crate::{fetch_bytes, fetch_verified, Expect, FetchError, FetchOpts, MAX_REDIRECTS};

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn opts() -> FetchOpts {
    FetchOpts {
        allow_loopback_http: true,
        ..FetchOpts::default()
    }
}

/// A temporary directory that is private, which `tempfile` does not promise.
fn private_tmp() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[tokio::test]
async fn a_matching_download_is_kept() {
    let body = b"the real archive".to_vec();
    let expected = body.clone();
    let server = serve(move |_, _| Reply::Ok(body.clone())).await;
    let dir = private_tmp();
    let dest = dir.path().join("a.tar.gz");
    let done = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(&expected),
            max_size: 1024,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(done.size, expected.len() as u64);
    assert_eq!(done.sha256, sha(&expected));
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
}

#[tokio::test]
async fn sha256_mismatch_deletes_the_file() {
    let server = serve(|_, _| Reply::Ok(b"tampered".to_vec())).await;
    let dir = private_tmp();
    let dest = dir.path().join("a.tar.gz");
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(b"original"),
            max_size: 1024,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::HashMismatch { .. }), "{err}");
    assert!(!dest.exists(), "the file of a failed download is deleted");
}

#[tokio::test]
async fn size_cap_cuts_the_download_midway() {
    // no Content-Length: only counting the bytes can stop it
    let server = serve(|_, _| Reply::Unbounded(8 * 1024 * 1024)).await;
    let dir = private_tmp();
    let dest = dir.path().join("a.tar.gz");
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: [0; 32],
            max_size: 64 * 1024,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::TooLarge { max: 65536 }), "{err}");
    assert!(!dest.exists());
    // the same for the in-memory variant
    let err = fetch_bytes(&server.url("/b"), 1000, &opts())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::TooLarge { .. }), "{err}");
}

#[tokio::test]
async fn content_length_larger_than_cap_refused_before_reading() {
    // the body never comes: only the header can be what refuses it, and quickly
    let server = serve(|_, _| Reply::Announce(10_000_000)).await;
    let dir = private_tmp();
    let dest = dir.path().join("a.tar.gz");
    let started = std::time::Instant::now();
    let quick = FetchOpts {
        total_timeout: std::time::Duration::from_secs(3),
        ..opts()
    };
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: [0; 32],
            max_size: 1000,
        },
        &dest,
        &quick,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            FetchError::TooLargeAnnounced {
                announced: 10_000_000,
                max: 1000
            }
        ),
        "{err}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "it waited for a body"
    );
    assert!(!dest.exists(), "no file is created for a refused length");
}

#[tokio::test]
async fn http_url_refused() {
    let server = serve(|_, _| Reply::Ok(b"x".to_vec())).await;
    let dir = private_tmp();
    let dest = dir.path().join("a");
    // with the default options, a loopback http URL is refused like any other
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(b"x"),
            max_size: 10,
        },
        &dest,
        &FetchOpts::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::NotHttps(_)), "{err}");
    for url in [
        "http://example.invalid/a",
        "ftp://example.invalid/a",
        "file:///etc/passwd",
    ] {
        let err = fetch_verified(
            url,
            &Expect {
                sha256: [0; 32],
                max_size: 10,
            },
            &dest,
            &FetchOpts::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FetchError::NotHttps(_)), "{url}: {err}");
    }
    assert!(
        server.requests().is_empty(),
        "a refused URL is never requested"
    );
    assert!(!dest.exists());
    let err = fetch_bytes("http://example.invalid/a", 10, &FetchOpts::default())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::NotHttps(_)), "{err}");
}

#[tokio::test]
async fn redirect_to_http_refused() {
    let server = serve(|path, _| match path {
        "/start" => Reply::Status(
            302,
            vec![("Location".into(), "http://203.0.113.9/payload".into())],
        ),
        "/ftp" => Reply::Status(
            302,
            vec![("Location".into(), "ftp://203.0.113.9/payload".into())],
        ),
        _ => Reply::Ok(b"fine".to_vec()),
    })
    .await;
    let dir = private_tmp();
    for path in ["/start", "/ftp"] {
        let dest = dir.path().join("a");
        let err = fetch_verified(
            &server.url(path),
            &Expect {
                sha256: sha(b"fine"),
                max_size: 100,
            },
            &dest,
            &opts(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, FetchError::RedirectNotHttps(_)),
            "{path}: {err}"
        );
        assert!(!dest.exists());
    }
}

#[tokio::test]
async fn redirect_limit_is_three() {
    // a loop: /r keeps pointing at itself; the original request plus three hops are made
    let looping = serve(|_, _| Reply::Status(302, vec![("Location".into(), "/r".into())])).await;
    let dir = private_tmp();
    let dest = dir.path().join("a");
    let err = fetch_verified(
        &looping.url("/r"),
        &Expect {
            sha256: [0; 32],
            max_size: 100,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::TooManyRedirects(3)), "{err}");
    assert_eq!(
        looping.requests().len(),
        MAX_REDIRECTS + 1,
        "{:?}",
        looping.requests()
    );

    // a chain of exactly three redirects is followed
    let chain = serve(|path, _| match path {
        "/0" => Reply::Status(302, vec![("Location".into(), "/1".into())]),
        "/1" => Reply::Status(302, vec![("Location".into(), "/2".into())]),
        "/2" => Reply::Status(302, vec![("Location".into(), "/3".into())]),
        _ => Reply::Ok(b"end".to_vec()),
    })
    .await;
    let dest = dir.path().join("b");
    fetch_verified(
        &chain.url("/0"),
        &Expect {
            sha256: sha(b"end"),
            max_size: 100,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(chain.requests().len(), 4);
}

#[tokio::test]
async fn file_is_created_0600_in_0700_dir() {
    let server = serve(|_, _| Reply::Ok(b"data".to_vec())).await;
    let root = private_tmp();
    // a missing directory is created private, with its missing parents
    let dest = root.path().join("staging/random/a.tar.gz");
    fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(b"data"),
            max_size: 100,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(mode_of(&dest), 0o600);
    assert_eq!(mode_of(dest.parent().unwrap()), 0o700);
    assert_eq!(mode_of(dest.parent().unwrap().parent().unwrap()), 0o700);

    // a directory that is there but open to others is refused, and nothing is requested
    let open = root.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = server.requests().len();
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(b"data"),
            max_size: 100,
        },
        &open.join("a"),
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::Unsafe { .. }), "{err}");
    assert_eq!(server.requests().len(), before);

    // an existing file is never overwritten (and so never followed through a link)
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: sha(b"data"),
            max_size: 100,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::Io { .. }), "{err}");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"data",
        "the earlier file is untouched"
    );
}

#[tokio::test]
async fn an_error_status_is_an_error() {
    let server = serve(|_, _| Reply::Status(404, vec![])).await;
    let dir = private_tmp();
    let dest = dir.path().join("a");
    let err = fetch_verified(
        &server.url("/a"),
        &Expect {
            sha256: [0; 32],
            max_size: 100,
        },
        &dest,
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::Status(404)), "{err}");
    assert!(!dest.exists());
}
