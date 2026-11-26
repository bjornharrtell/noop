//! Replacing this binary with the newest release.
//!
//! The release carries `noop-agent-<arch>-linux.tar.gz` and `SHA256SUMS`. Both are downloaded into the home directory
//! (never `/tmp`: on a small VPS that is a 464 MB tmpfs), the archive is checked against the checksum, and only then is
//! the binary renamed over the running one — a rename is atomic, so a crash cannot leave half a program behind. The
//! caller restarts the service; systemd's `Restart=always` does that when the process exits.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Where the releases come from; `NOOP_REPO` overrides (a fork, a mirror).
pub const REPO: &str = "bjornharrtell/noop";

/// The archive for a build, as the workflow names it: `noop-agent-<arch>-<os>.tar.gz`.
pub fn asset_name_for(arch: &str, os: &str) -> String {
    let arch = match arch {
        "aarch64" | "arm64" => "aarch64",
        _ => "x86_64",
    };
    let os = if os == "macos" { "darwin" } else { "linux" };
    format!("noop-agent-{arch}-{os}.tar.gz")
}

/// The archive for this machine.
pub fn asset_name() -> String {
    asset_name_for(std::env::consts::ARCH, std::env::consts::OS)
}

/// `server-v0.1.9` → `0.1.9`.
pub fn version_of(tag: &str) -> &str {
    tag.strip_prefix("server-v").or_else(|| tag.strip_prefix('v')).unwrap_or(tag)
}

/// Compare dotted versions, numerically per part: `0.1.10` is newer than `0.1.9`.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// The checksum for `name` out of a `SHA256SUMS` file (`<hex>  <name>`, two spaces as `sha256sum` writes it).
pub fn checksum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let (hash, file) = (it.next()?, it.next()?);
        (file.trim_start_matches('*') == name && hash.len() == 64).then_some(hash)
    })
}

fn sha256hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Unpack `noop` out of the release archive.
pub fn binary_from_tarball(targz: &[u8]) -> Result<Vec<u8>, String> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(targz));
    for entry in ar.entries().map_err(|e| format!("archive: {e}"))? {
        let mut e = entry.map_err(|e| format!("archive: {e}"))?;
        let path = e.path().map_err(|e| format!("archive: {e}"))?.to_path_buf();
        if path.file_name().and_then(|n| n.to_str()) == Some("noop") {
            let mut out = Vec::new();
            e.read_to_end(&mut out).map_err(|e| format!("archive: {e}"))?;
            return Ok(out);
        }
    }
    Err("the archive holds no `noop`".into())
}

/// What an update did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Already on this version.
    Current(String),
    /// Replaced the binary; the caller must restart to use it.
    Updated { from: String, to: String },
}

/// Download the newest release and replace this binary, unless `current` is already it.
pub async fn run(home: &Path, current: &str, force: bool) -> Result<Outcome, String> {
    let repo = std::env::var("NOOP_REPO").unwrap_or_else(|_| REPO.to_string());
    let http = reqwest::Client::builder()
        .user_agent("noop")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let rel: serde_json::Value = http
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        .send()
        .await
        .map_err(|e| format!("release info: {e}"))?
        .error_for_status()
        .map_err(|e| format!("release info: {e}"))?
        .json()
        .await
        .map_err(|e| format!("release info: {e}"))?;
    let tag = rel.get("tag_name").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let version = version_of(&tag).to_string();
    if !force && !newer(&version, current) {
        return Ok(Outcome::Current(current.to_string()));
    }
    let asset = asset_name();
    let url = |name: &str| {
        rel.get("assets")
            .and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(name)))
            .and_then(|x| x.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("release {tag} has no {name}"))
    };
    let (tarball_url, sums_url) = (url(&asset)?, url("SHA256SUMS")?);

    let dir: PathBuf = home.join("update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let get = |u: String| {
        let http = http.clone();
        async move {
            let r = http.get(u).send().await.map_err(|e| format!("download: {e}"))?;
            let r = r.error_for_status().map_err(|e| format!("download: {e}"))?;
            r.bytes().await.map(|b| b.to_vec()).map_err(|e| format!("download: {e}"))
        }
    };
    let sums = String::from_utf8(get(sums_url).await?).map_err(|_| "SHA256SUMS is not text")?;
    let wanted = checksum_for(&sums, &asset).ok_or_else(|| format!("SHA256SUMS names no {asset}"))?.to_string();
    let tarball = get(tarball_url).await?;
    let got = sha256hex(&tarball);
    if got != wanted {
        return Err(format!("checksum mismatch for {asset}: expected {wanted}, got {got}"));
    }
    let binary = binary_from_tarball(&tarball)?;
    let staged = dir.join("noop.new");
    std::fs::write(&staged, &binary).map_err(|e| format!("{}: {e}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("current exe: {e}"))?;
    std::fs::rename(&staged, &exe).map_err(|e| format!("replace {}: {e}", exe.display()))?;
    Ok(Outcome::Updated { from: current.to_string(), to: version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_follow_the_platform() {
        assert_eq!(asset_name_for("x86_64", "linux"), "noop-agent-x86_64-linux.tar.gz");
        assert_eq!(asset_name_for("aarch64", "linux"), "noop-agent-aarch64-linux.tar.gz");
        assert_eq!(asset_name_for("aarch64", "macos"), "noop-agent-aarch64-darwin.tar.gz");
        assert_eq!(asset_name_for("arm64", "macos"), "noop-agent-aarch64-darwin.tar.gz");
        assert_eq!(asset_name_for("x86_64", "macos"), "noop-agent-x86_64-darwin.tar.gz");
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("0.2.0", "0.1.99"));
        assert!(!newer("0.1.8", "0.1.9"));
        assert!(!newer("0.1.9", "0.1.9"));
        assert_eq!(version_of("server-v0.1.9"), "0.1.9");
        assert_eq!(version_of("v2.0.0"), "2.0.0");
    }

    #[test]
    fn checksums_are_found_by_name() {
        let h = "a".repeat(64);
        let sums = format!("{h}  noop-server-x86_64-linux.tar.gz\n{h}  noop-agent-x86_64-linux.tar.gz\n");
        assert_eq!(checksum_for(&sums, "noop-agent-x86_64-linux.tar.gz"), Some(h.as_str()));
        assert_eq!(checksum_for(&sums, "noop-agent-aarch64-linux.tar.gz"), None);
        assert_eq!(checksum_for(&sums, "noop-agent-x86_64-linux.tar.gz").map(str::len), Some(64));
    }

    #[test]
    fn the_binary_is_taken_out_of_the_archive() {
        // build a tiny archive holding ./noop and read it back
        let mut data = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut data, flate2::Compression::fast());
            let mut b = tar::Builder::new(enc);
            let body = b"#!/bin/sh\necho hi\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "./noop", &body[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let got = binary_from_tarball(&data).unwrap();
        assert!(got.starts_with(b"#!/bin/sh"));
        assert!(binary_from_tarball(b"not an archive").is_err());
    }
}
