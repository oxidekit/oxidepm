//! Prepare and verify release inputs, and build revisions of the adapter catalogue.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use chrono::{DateTime, Days, NaiveDate, Utc};
use oxidepm_adapter_proto::signing::OFFICIAL_INDEX_KEYS;
use oxidepm_adapter_proto::{
    trusted_comment_for, verify_index, verify_minisign, Artifact, Index, IndexAdapter, IndexKey,
    Release, INDEX_MAX_BYTES,
};
use semver::Version;
use sha2::{Digest, Sha256};

const TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];
const INPUT: &str = "adapter-catalogue-input.json";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(error) = run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("catalogue: {error}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    match args {
        [command, repo, tag, date, bundles, out, names @ ..] if command == "prepare" => {
            prepare(repo, tag, date, bundles, out, names)?;
        }
        [command, repo, tag, current, input, out] if command == "advance" => {
            advance(repo, tag, current, input, out)?;
        }
        [command, current, out] if command == "renew" => {
            let index = read_index(current)?;
            let now = Utc::now();
            if index.expires < now + Days::new(60) {
                let sequence = index.sequence;
                write_revision(index, sequence, now, out)?;
            }
        }
        [command, current] if command == "check_expiry" => {
            let index = read_index(current)?;
            if index.expires < Utc::now() + Days::new(45) {
                return Err(format!("the catalogue expires at {} (less than 45 days remain)", index.expires).into());
            }
        }
        [command, index, signature, key, previous] if command == "verify_pair" => {
            let body = read_limited(index, INDEX_MAX_BYTES)?;
            let signature = String::from_utf8(read_limited(signature, 4096)?)?;
            let verified = verify_index(&body, &signature, &[public_key(key)?])?;
            let expected = next_sequence(previous.parse()?)?;
            if verified.index.sequence != expected {
                return Err(format!(
                    "catalogue sequence {} does not follow {}",
                    verified.index.sequence, previous
                )
                .into());
            }
            println!("{expected}");
        }
        [command, sums, signature, key, tag] if command == "verify_checksums" => {
            let body = read_limited(sums, 64 * 1024)?;
            let signature = String::from_utf8(read_limited(signature, 4096)?)?;
            let signed = verify_minisign(&body, &signature, &[public_key(key)?])?;
            if signed.trusted_comment != format!("oxidepm release checksums tag={tag}") {
                return Err("release signature has an unexpected trusted comment".into());
            }
            for (name, hash) in checksums(&body)? {
                println!("{hash}  {name}");
            }
        }
        [command, repo, index] if command == "assets" => {
            for (url, artifact) in pinned_assets(repo, &read_index(index)?)? {
                println!("{} {} {}", artifact.sha256, artifact.size, url);
            }
        }
        _ => {
            return Err("usage: catalogue prepare|advance|renew|verify_pair|verify_checksums|assets|check_expiry ...".into())
        }
    }
    Ok(())
}

fn public_key(key: &str) -> Result<IndexKey> {
    Ok(IndexKey::parse(if key == "-" {
        OFFICIAL_INDEX_KEYS[0]
    } else {
        key
    })?)
}

fn read_limited(path: impl AsRef<Path>, max: usize) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(format!("{} exceeds the size limit", path.display()).into());
    }
    Ok(bytes)
}

fn read_index(path: impl AsRef<Path>) -> Result<Index> {
    Ok(Index::parse(&read_limited(path, INDEX_MAX_BYTES)?)?)
}

fn tag_version(tag: &str) -> Result<Version> {
    Ok(Version::parse(
        tag.strip_prefix('v')
            .ok_or("release tag must start with v")?,
    )?)
}

fn release_prefix(repo: &str) -> Result<String> {
    let slug = |part: &str| {
        !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !repo
        .split_once('/')
        .is_some_and(|(owner, name)| slug(owner) && slug(name))
    {
        return Err("expected a GitHub repository in OWNER/REPO format".into());
    }
    Ok(format!("https://github.com/{repo}/releases/download/"))
}

fn checksums(body: &[u8]) -> Result<BTreeMap<&str, &str>> {
    let mut sums = BTreeMap::new();
    for line in std::str::from_utf8(body)?.lines() {
        let (hash, name) = line.split_once("  ").ok_or("invalid checksum line")?;
        if !oxidepm_adapter_proto::text::is_sha256_hex(hash)
            || matches!(name, "" | "." | "..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        {
            return Err(format!("invalid checksum line for {name:?}").into());
        }
        if sums.insert(name, hash).is_some() {
            return Err(format!("duplicate checksum for {name}").into());
        }
    }
    Ok(sums)
}

fn asset_name(adapter: &str, target: &str) -> String {
    format!("oxidepm-adapter-{adapter}-{target}.tar.gz")
}

fn copy_asset(source: &Path, out: &Path, name: &str) -> Result<()> {
    let destination = out.join(name);
    if destination.exists() {
        return Err(format!("colliding release asset basename {name}").into());
    }
    let metadata = source
        .symlink_metadata()
        .map_err(|error| format!("missing asset {}: {error}", source.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{} is not a nonempty regular file", source.display()).into());
    }
    fs::copy(source, destination)?;
    Ok(())
}

fn prepare(
    repo: &str,
    tag: &str,
    date: &str,
    bundles: &str,
    out: &str,
    names: &[String],
) -> Result<()> {
    tag_version(tag)?;
    let base = format!("{}{tag}/", release_prefix(repo)?);
    // Use the tagged commit's date across the matrix, which can finish on different UTC days.
    let released = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let generated = released
        .and_hms_opt(0, 0, 0)
        .ok_or("invalid release date")?
        .and_utc();
    let expected_names: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    if expected_names.is_empty() || expected_names.len() != names.len() {
        return Err("expected a nonempty list of distinct adapter names".into());
    }
    let bundles = Path::new(bundles);
    let out = Path::new(out);
    for target in TARGETS {
        if !bundles.join(format!("oxidepm-{target}")).is_dir() {
            return Err(format!("missing target {target}").into());
        }
    }
    fs::create_dir(out)?;
    let mut merged: BTreeMap<String, IndexAdapter> = BTreeMap::new();
    for target in TARGETS {
        let packages = bundles.join(format!("oxidepm-{target}"));
        for name in [
            format!("oxidepm-{target}.tar.gz"),
            format!("oxidepm-adapters-{target}.tar.gz"),
        ] {
            copy_asset(&packages.join(&name), out, &name)?;
        }
        let arch = if target.starts_with("x86_64") {
            "amd64"
        } else {
            "arm64"
        };
        let name = format!("oxidepm_{}_{arch}.deb", tag[1..].replace('-', "."));
        copy_asset(&packages.join(&name), out, &name)?;
        let bundle = packages.join("adapters");
        let body = read_limited(bundle.join("index.json"), INDEX_MAX_BYTES)?;
        let checksum_bytes = read_limited(bundle.join("SHA256SUMS"), 64 * 1024)?;
        let mut sums = checksums(&checksum_bytes)?;
        if sums.remove("index.json") != Some(format!("{:x}", Sha256::digest(&body)).as_str()) {
            return Err(format!("{target}: bundle index checksum disagrees").into());
        }
        let index = Index::parse(&body)?;
        let actual_names: BTreeSet<&str> = index
            .adapters
            .iter()
            .map(|adapter| adapter.name.as_str())
            .collect();
        if actual_names != expected_names || !index.revoked.is_empty() {
            return Err(
                format!("{target}: unexpected adapter list or revocations in bundle").into(),
            );
        }
        let mut basenames = BTreeSet::new();
        for mut adapter in index.adapters {
            let [release] = adapter.releases.as_mut_slice() else {
                return Err(format!("{target}: expected one release of {}", adapter.name).into());
            };
            let name = format!("{}-{}.tar.gz", adapter.name, release.version);
            if !basenames.insert(name.clone()) {
                return Err(format!("{target}: colliding bundle archive basename {name}").into());
            }
            if release.yanked || release.yank_reason.is_some() || release.artifacts.len() != 1 {
                return Err(format!("{target}: invalid bundle release of {}", adapter.name).into());
            }
            let mut artifact = release
                .artifacts
                .remove(target)
                .ok_or_else(|| format!("{target}: no artifact for {}", adapter.name))?;
            if artifact.url != format!("{base}{name}") {
                return Err(format!("{target}: unexpected bundle archive URL for {name}").into());
            }
            if sums.remove(name.as_str()) != Some(artifact.sha256.as_str()) {
                return Err(format!("{target}: archive checksum disagrees for {name}").into());
            }
            let asset = asset_name(&adapter.name, target);
            copy_asset(&bundle.join(&name), out, &asset)?;
            let mut hasher = Sha256::new();
            let size = io::copy(&mut File::open(out.join(&asset))?, &mut hasher)?;
            if size != artifact.size || format!("{:x}", hasher.finalize()) != artifact.sha256 {
                return Err(format!("{target}: archive size or hash disagrees for {name}").into());
            }
            artifact.url = format!("{base}{asset}");
            release.released = released;

            if let Some(previous) = merged.get_mut(&adapter.name) {
                // The index and release fields must agree before joining target artifacts.
                let mut metadata = previous.clone();
                metadata.releases[0].artifacts.clear();
                if metadata != adapter {
                    return Err(format!(
                        "{target}: release metadata disagrees for {}",
                        adapter.name
                    )
                    .into());
                }
                previous.releases[0]
                    .artifacts
                    .insert(target.to_owned(), artifact);
            } else {
                release.artifacts.insert(target.to_owned(), artifact);
                merged.insert(adapter.name.clone(), adapter);
            }
        }
        if !sums.is_empty() {
            return Err(format!("{target}: unexpected bundle checksums").into());
        }
    }
    let index = Index {
        schema: 1,
        sequence: 1,
        generated,
        expires: generated + Days::new(90),
        revoked: Vec::new(),
        adapters: merged.into_values().collect(),
    };
    pinned_assets(repo, &index)?;
    write_index(&index, &out.join(INPUT))?;
    Ok(())
}

fn pinned_assets<'a>(repo: &str, index: &'a Index) -> Result<BTreeMap<&'a str, &'a Artifact>> {
    let prefix = release_prefix(repo)?;
    let mut assets = BTreeMap::new();
    for adapter in &index.adapters {
        for release in &adapter.releases {
            for (target, artifact) in &release.artifacts {
                let tag = pinned_tag(&prefix, artifact)?;
                if artifact.url != format!("{prefix}{tag}/{}", asset_name(&adapter.name, target)) {
                    return Err(format!("unexpected release asset URL: {}", artifact.url).into());
                }
                if assets.insert(artifact.url.as_str(), artifact).is_some() {
                    return Err(format!("colliding release asset URL: {}", artifact.url).into());
                }
            }
        }
    }
    Ok(assets)
}

fn pinned_tag<'a>(prefix: &str, artifact: &'a Artifact) -> Result<&'a str> {
    let (tag, _) = artifact
        .url
        .strip_prefix(prefix)
        .and_then(|path| path.split_once('/'))
        .ok_or_else(|| format!("not a tag-pinned release asset: {}", artifact.url))?;
    tag_version(tag)?;
    Ok(tag)
}

fn same_artifacts(current: &Release, incoming: &Release) -> bool {
    // The incoming release has exactly the required targets. Older records can have more.
    incoming.artifacts.iter().all(|(target, artifact)| {
        current.artifacts.get(target).is_some_and(|previous| {
            let mut artifact = artifact.clone();
            artifact.url.clone_from(&previous.url);
            &artifact == previous
        })
    })
}

fn advance(repo: &str, tag: &str, current: &str, input: &str, out: &str) -> Result<()> {
    let mut next = read_index(input)?;
    let prefix = release_prefix(repo)?;
    let version = tag_version(tag)?;
    if next.adapters.is_empty() || !next.revoked.is_empty() {
        return Err("expected release input with adapters and no revocations".into());
    }
    for adapter in &next.adapters {
        let [release] = adapter.releases.as_slice() else {
            return Err(format!("expected one release of {}", adapter.name).into());
        };
        if release.yanked
            || release.yank_reason.is_some()
            || release.artifacts.len() != TARGETS.len()
            || TARGETS
                .iter()
                .any(|target| !release.artifacts.contains_key(*target))
        {
            return Err(format!("missing target or invalid release of {}", adapter.name).into());
        }
    }
    for artifact in pinned_assets(repo, &next)?.values() {
        if pinned_tag(&prefix, artifact)? != tag {
            return Err("release input points outside the requested tag".into());
        }
    }
    let now = Utc::now();
    if current == "-" {
        return write_revision(next, 0, now, out);
    }

    let previous = read_index(current)?;
    for artifact in pinned_assets(repo, &previous)?.values() {
        if version
            .cmp_precedence(&tag_version(pinned_tag(&prefix, artifact)?)?)
            .is_lt()
        {
            return Err(
                format!("older release tag {tag} cannot replace the current catalogue").into(),
            );
        }
    }
    let mut changed = next.adapters.len() != previous.adapters.len();
    for old in &previous.adapters {
        let new = next
            .adapters
            .iter_mut()
            .find(|adapter| adapter.name == old.name)
            .ok_or_else(|| format!("release dropped adapter {}", old.name))?;
        let candidate = &new.releases[0];
        let candidate_version = Version::parse(&candidate.version)?;
        let mut unchanged = false;
        for release in &old.releases {
            let ordering = candidate_version.cmp_precedence(&Version::parse(&release.version)?);
            if ordering.is_lt() {
                return Err(
                    format!("older adapter release {} {}", old.name, candidate.version).into(),
                );
            }
            if ordering.is_eq() {
                if candidate.version != release.version || !same_artifacts(release, candidate) {
                    return Err(format!(
                        "{} {} changed at the same version",
                        old.name, candidate.version
                    )
                    .into());
                }
                unchanged = true;
            }
        }
        if unchanged {
            // Keep the published record, including artifacts for previously supported targets.
            new.clone_from(old);
        } else {
            changed = true;
        }
    }
    if changed {
        next.revoked = previous.revoked;
        write_revision(next, previous.sequence, now, out)?;
    }
    Ok(())
}

fn next_sequence(previous: u64) -> Result<u64> {
    previous
        .checked_add(1)
        .ok_or_else(|| "catalogue sequence exhausted".into())
}

fn write_revision(mut index: Index, previous: u64, now: DateTime<Utc>, out: &str) -> Result<()> {
    index.sequence = next_sequence(previous)?;
    index.generated = DateTime::from_timestamp(now.timestamp(), 0).ok_or("invalid time")?;
    index.expires = index.generated + Days::new(90);
    write_index(&index, Path::new(out))?;
    println!("{}", trusted_comment_for(&index));
    Ok(())
}

fn write_index(index: &Index, path: &Path) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(index)?;
    body.push(b'\n');
    Index::parse(&body)?;
    fs::write(path, body)?;
    Ok(())
}
