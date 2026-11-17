//! Explicit, read-only advisory lookup of immutable public-registry lock entries.
//! No package manager, resolution, checkout, or plugin process is involved.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Finding, Severity};
use crate::git::{Git, Mirror};

const OSV_ENDPOINT: &str = "https://api.osv.dev/v1/querybatch";
const MAX_BLOB: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BLOBS: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_PACKAGES: usize = 4096;
const BATCH_SIZE: usize = 64;
const SCAN_TIME: Duration = Duration::from_secs(90);
const UNSUPPORTED_LOCKS: &[&str] = &[
    "yarn.lock",
    "pnpm-lock.yaml",
    "npm-shrinkwrap.json",
    "bun.lock",
    "bun.lockb",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyReport {
    pub complete: bool,
    /// Distinct public ecosystem/name/version queries with a complete response.
    pub checked: usize,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

impl DependencyReport {
    fn new() -> Self {
        Self {
            complete: true,
            checked: 0,
            findings: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn incomplete(&mut self, note: &str) {
        self.complete = false;
        if !self.notes.iter().any(|existing| existing == note) {
            self.notes.push(note.to_owned());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Package {
    ecosystem: &'static str,
    name: String,
    version: String,
}

type Packages = BTreeMap<Package, BTreeSet<String>>;

/// Called only for an explicitly requested dependency check. Requests contain
/// public package identity/version only, never lockfile text, source, or URLs.
pub fn check(_git: Git, mirror: &Mirror, commit: &str, subdir: Option<&str>) -> DependencyReport {
    let started = Instant::now();
    let mut report = DependencyReport::new();
    let Some(directories) = ancestor_directories(subdir) else {
        report.incomplete("Dependency lockfile scope is invalid or exceeds the directory limit.");
        return report;
    };
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|c| c.is_ascii_hexdigit()) {
        report.incomplete("Dependency checks require an exact immutable Git commit.");
        return report;
    }
    // Force local subprocess bounds independently of interactive caller settings.
    let git = Git::BACKGROUND;
    let mut packages = Packages::new();
    let mut found = false;
    let mut total_bytes = 0;
    for directory in directories {
        if started.elapsed() >= SCAN_TIME {
            report.incomplete("Dependency audit reached its time limit.");
            break;
        }
        let paths: Vec<String> = ["Cargo.lock", "package-lock.json"]
            .iter()
            .chain(UNSUPPORTED_LOCKS.iter())
            .map(|name| {
                if directory.is_empty() {
                    (*name).to_owned()
                } else {
                    format!("{directory}/{name}")
                }
            })
            .collect();
        let mut args = vec!["--literal-pathspecs", "ls-tree", "-z", commit, "--"];
        args.extend(paths.iter().map(String::as_str));
        let Ok(listing) = git.run_bytes(Some(&mirror.path), &args, false) else {
            report.incomplete("Dependency lockfile discovery could not read the Git tree.");
            continue;
        };
        // Literal pathspecs restrict this to at most the requested lockfiles.
        for entry in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let Some((metadata, path)) = entry.split_once_byte(b'\t') else {
                report.incomplete("Dependency lockfile Git metadata is malformed.");
                continue;
            };
            let (Ok(metadata), Ok(path)) =
                (std::str::from_utf8(metadata), std::str::from_utf8(path))
            else {
                report.incomplete("Dependency lockfile Git metadata could not be decoded.");
                continue;
            };
            if !paths.iter().any(|expected| expected == path) {
                report.incomplete("Dependency lockfile discovery returned an unexpected path.");
                continue;
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            if UNSUPPORTED_LOCKS.contains(&name) {
                report.incomplete("An unsupported dependency lockfile is present; its dependencies were not checked.");
                continue;
            }
            found = true;
            let fields: Vec<&str> = metadata.split(' ').collect();
            if fields.len() != 3
                || !matches!(fields[0], "100644" | "100755")
                || fields[1] != "blob"
                || !matches!(fields[2].len(), 40 | 64)
                || !fields[2].bytes().all(|c| c.is_ascii_hexdigit())
            {
                report.incomplete("A dependency lockfile is not an inspectable regular Git blob.");
                continue;
            }
            let oid = fields[2];
            let size = git
                .run(Some(&mirror.path), &["cat-file", "-s", oid], false)
                .ok()
                .and_then(|s| s.parse::<usize>().ok());
            let Some(size) = size else {
                report.incomplete("A dependency lockfile size could not be read.");
                continue;
            };
            if size > MAX_BLOB || size > MAX_TOTAL_BLOBS - total_bytes {
                report.incomplete("Dependency lockfile bytes exceed the audit size limit.");
                continue;
            }
            total_bytes += size;
            let Ok(bytes) = git.run_bytes(Some(&mirror.path), &["cat-file", "blob", oid], false)
            else {
                report.incomplete("A dependency lockfile could not be read from Git.");
                continue;
            };
            if bytes.len() != size {
                report.incomplete("A dependency lockfile blob has an unexpected size.");
                continue;
            }
            match name {
                "Cargo.lock" => parse_cargo(&bytes, path, &mut packages, &mut report),
                "package-lock.json" => parse_npm(&bytes, path, &mut packages, &mut report),
                _ => unreachable!(),
            }
        }
    }
    if !found {
        report.incomplete(
            "No supported dependency lockfile was found in the plugin or its workspace ancestors.",
        );
    }
    let agent = osv_agent();
    query_packages(&packages, &mut report, started, |request, remaining| {
        send_batch(&agent, OSV_ENDPOINT, request, remaining)
    });
    report.notes.push(format!("{} distinct public dependency versions received complete advisory responses. Advisory coverage does not establish code safety.", report.checked));
    report
}

// A tiny byte helper keeps Git's NUL-delimited paths exact without lossy text.
trait SplitByte {
    fn split_once_byte(&self, byte: u8) -> Option<(&[u8], &[u8])>;
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, byte: u8) -> Option<(&[u8], &[u8])> {
        let position = self.iter().position(|c| *c == byte)?;
        Some((&self[..position], &self[position + 1..]))
    }
}

fn ancestor_directories(subdir: Option<&str>) -> Option<Vec<String>> {
    let subdir = subdir.unwrap_or("").trim_end_matches('/');
    if subdir.contains('\\') || subdir.starts_with('/') || subdir.chars().any(char::is_control) {
        return None;
    }
    let mut components: Vec<&str> = if subdir.is_empty() {
        Vec::new()
    } else {
        subdir.split('/').collect()
    };
    if components.len() > 32
        || components
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return None;
    }
    let mut directories = Vec::with_capacity(components.len() + 1);
    loop {
        directories.push(components.join("/"));
        if components.pop().is_none() {
            break;
        }
    }
    Some(directories)
}

fn valid_name(name: &str, ecosystem: &str) -> bool {
    fn component(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    }
    if name.len() > 214 {
        return false;
    }
    if ecosystem == "npm" && name.starts_with('@') {
        return name[1..]
            .split_once('/')
            .is_some_and(|(scope, name)| component(scope) && component(name));
    }
    component(name)
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version.as_bytes()[0].is_ascii_digit()
        && version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
}

fn add_package(
    ecosystem: &'static str,
    name: &str,
    version: &str,
    path: &str,
    packages: &mut Packages,
    report: &mut DependencyReport,
) {
    if !valid_name(name, ecosystem) || !valid_version(version) {
        report.incomplete("A public dependency has an unsupported name or non-exact version; it was not sent to the advisory service.");
        return;
    }
    let package = Package {
        ecosystem,
        name: name.to_owned(),
        version: version.to_owned(),
    };
    if packages.len() >= MAX_PACKAGES && !packages.contains_key(&package) {
        report.incomplete("Dependency count exceeds the advisory audit limit.");
        return;
    }
    packages.entry(package).or_default().insert(path.to_owned());
}

#[derive(Deserialize)]
struct CargoLock {
    version: Option<u64>,
    package: Vec<CargoPackage>,
}
#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    source: Option<String>,
}

fn parse_cargo(bytes: &[u8], path: &str, packages: &mut Packages, report: &mut DependencyReport) {
    let parsed = std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| toml::from_str::<CargoLock>(text).ok());
    let Some(lock) = parsed else {
        report.incomplete(
            "A Cargo.lock file could not be parsed; dependency coverage is incomplete.",
        );
        return;
    };
    if !matches!(lock.version, None | Some(1..=4)) {
        report.incomplete("A Cargo.lock format version is unsupported.");
        return;
    }
    for package in lock.package {
        if matches!(
            package.source.as_deref(),
            Some(
                "registry+https://github.com/rust-lang/crates.io-index"
                    | "sparse+https://index.crates.io/"
            )
        ) {
            add_package(
                "crates.io",
                &package.name,
                &package.version,
                path,
                packages,
                report,
            );
        } else {
            report.incomplete("Local, Git, or non-public-registry Cargo packages remain unverified and were not sent to the advisory service.");
        }
    }
}

fn parse_npm(bytes: &[u8], path: &str, packages: &mut Packages, report: &mut DependencyReport) {
    let Ok(lock) = serde_json::from_slice::<Value>(bytes) else {
        report.incomplete(
            "A package-lock.json file could not be parsed; dependency coverage is incomplete.",
        );
        return;
    };
    match lock.get("lockfileVersion").and_then(Value::as_u64) {
        Some(1) => {
            if let Some(dependencies) = lock.get("dependencies").and_then(Value::as_object) {
                npm_v1(dependencies, path, packages, report, 0);
            } else {
                report.incomplete("An npm v1 lockfile has no usable dependency table.");
            }
        }
        Some(2 | 3) => {
            let Some(entries) = lock.get("packages").and_then(Value::as_object) else {
                report.incomplete("An npm lockfile has no usable package table.");
                return;
            };
            for (location, entry) in entries {
                if location.is_empty() {
                    continue;
                } // Root project metadata, not a resolved dependency.
                let name = location
                    .rsplit_once("node_modules/")
                    .filter(|(prefix, _)| prefix.is_empty() || prefix.ends_with('/'))
                    .map(|(_, name)| name);
                match name {
                    Some(name) if valid_name(name, "npm") => npm_entry(name, entry, path, packages, report),
                    _ => report.incomplete("Local or unsupported npm workspace packages remain unverified and were not sent to the advisory service."),
                }
            }
        }
        _ => report.incomplete("An npm lockfile format version is absent or unsupported."),
    }
}

fn npm_v1(
    entries: &serde_json::Map<String, Value>,
    path: &str,
    packages: &mut Packages,
    report: &mut DependencyReport,
    depth: usize,
) {
    if depth >= 64 {
        report.incomplete("Nested npm dependencies exceed the audit depth limit.");
        return;
    }
    for (name, entry) in entries {
        npm_entry(name, entry, path, packages, report);
        if let Some(nested) = entry.get("dependencies") {
            if let Some(nested) = nested.as_object() {
                npm_v1(nested, path, packages, report, depth + 1);
            } else {
                report.incomplete("An npm dependency table is malformed.");
            }
        }
    }
}

fn npm_entry(
    name: &str,
    entry: &Value,
    path: &str,
    packages: &mut Packages,
    report: &mut DependencyReport,
) {
    // An explicit public-registry origin is required. Missing origins (including
    // npm's omit-lockfile-registry-resolved output) must never leak private names.
    if entry.get("private").and_then(Value::as_bool) == Some(true)
        || entry.get("link").and_then(Value::as_bool) == Some(true)
    {
        report.incomplete("Private or linked npm dependencies remain unverified and were not sent to the advisory service.");
        return;
    }
    let Some(resolved) = entry.get("resolved").and_then(Value::as_str) else {
        report.incomplete("An npm dependency has no explicit public-registry origin and was not sent to the advisory service.");
        return;
    };
    if !resolved.starts_with("https://registry.npmjs.org/")
        || resolved.contains(['?', '#', '\\'])
        || resolved.chars().any(char::is_control)
    {
        report.incomplete("Git, private, or non-public-registry npm dependencies remain unverified and were not sent to the advisory service.");
        return;
    }
    let Some(version) = entry.get("version").and_then(Value::as_str) else {
        report.incomplete("An npm dependency has no exact version.");
        return;
    };
    // npm aliases may install a different public package under the path name.
    let name = entry.get("name").and_then(Value::as_str).unwrap_or(name);
    add_package("npm", name, version, path, packages, report);
}

fn osv_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_per_read(Some(Duration::from_secs(15)))
        .timeout_per_write(Some(Duration::from_secs(10)))
        .max_redirects(0)
        .proxy(None)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

fn send_batch(
    agent: &ureq::Agent,
    endpoint: &str,
    request: &Value,
    remaining: Duration,
) -> Result<Value, ()> {
    let response = agent
        .post(endpoint)
        .config()
        .timeout_global(Some(remaining.min(Duration::from_secs(20))))
        .build()
        .send_json(request)
        .map_err(|_| ())?;
    if response.status() != 200 {
        return Err(());
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take((MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_RESPONSE {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn query_packages<F>(
    packages: &Packages,
    report: &mut DependencyReport,
    started: Instant,
    mut send: F,
) where
    F: FnMut(&Value, Duration) -> Result<Value, ()>,
{
    let packages: Vec<_> = packages.iter().collect();
    for batch in packages.chunks(BATCH_SIZE) {
        let Some(remaining) = SCAN_TIME
            .checked_sub(started.elapsed())
            .filter(|time| !time.is_zero())
        else {
            report.incomplete("Dependency audit reached its time limit.");
            break;
        };
        let queries: Vec<_> = batch
            .iter()
            .map(|(package, _)| {
                json!({
                    "package": { "ecosystem": package.ecosystem, "name": package.name },
                    "version": package.version,
                })
            })
            .collect();
        let Ok(response) = send(&json!({ "queries": queries }), remaining) else {
            report.incomplete("The advisory service request failed or returned an unreadable or oversized response; no retry was attempted.");
            break;
        };
        let Some(results) = response
            .get("results")
            .and_then(Value::as_array)
            .filter(|results| results.len() == batch.len())
        else {
            report.incomplete("The advisory service response did not match the requested batch.");
            break;
        };
        for ((package, paths), result) in batch.iter().zip(results) {
            let Some(result) = result.as_object() else {
                report.incomplete("An advisory service result was malformed.");
                continue;
            };
            let paginated = match result.get("next_page_token") {
                None | Some(Value::Null) => false,
                Some(Value::String(token)) => !token.is_empty(),
                _ => true,
            };
            if paginated {
                report.incomplete("The advisory service returned a paginated result; remaining pages were not checked.");
            }
            let mut valid = !paginated;
            match result.get("vulns") {
                None => {} // OSV encodes an empty result as {}.
                Some(Value::Array(vulnerabilities)) => {
                    for vulnerability in vulnerabilities {
                        if vulnerability
                            .get("withdrawn")
                            .and_then(Value::as_str)
                            .is_some_and(|s| !s.is_empty())
                        {
                            continue;
                        }
                        let Some(id) = vulnerability
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| valid_advisory_id(id))
                        else {
                            valid = false;
                            report.incomplete(
                                "An advisory service vulnerability identifier was malformed.",
                            );
                            continue;
                        };
                        for path in *paths {
                            let finding = vulnerability_finding(package, path, id);
                            if !report
                                .findings
                                .iter()
                                .any(|existing| existing.id == finding.id)
                            {
                                report.findings.push(finding);
                            }
                        }
                    }
                }
                _ => {
                    valid = false;
                    report.incomplete("An advisory service vulnerability list was malformed.");
                }
            }
            if valid {
                report.checked += 1;
            }
        }
    }
}

fn valid_advisory_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn vulnerability_finding(package: &Package, path: &str, advisory: &str) -> Finding {
    Finding {
        id: super::fingerprint(
            "dependency-vulnerability",
            Some(path),
            &format!(
                "{}:{}:{}:{}",
                package.ecosystem, package.name, package.version, advisory
            ),
        ),
        rule: "dependency-vulnerability".to_owned(),
        severity: Severity::High,
        path: Some(path.to_owned()),
        line: None,
        message: format!("A locked public dependency has an active advisory: {advisory}. https://osv.dev/vulnerability/{advisory}"),
        is_new: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve_osv_once(response: String) -> (String, std::thread::JoinHandle<Value>) {
        use std::io::Write;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/querybatch", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
                assert!(headers.len() < 64 * 1024);
            }
            let headers = String::from_utf8(headers).unwrap().to_ascii_lowercase();
            assert!(headers.starts_with("post /querybatch http/1.1\r\n"));
            assert!(headers.lines().any(|line| {
                line.strip_prefix("content-type: ")
                    .is_some_and(|value| value.split(';').next() == Some("application/json"))
            }));
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert!(length < MAX_RESPONSE);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            serde_json::from_slice(&body).unwrap()
        });
        (endpoint, server)
    }

    #[test]
    fn osv_post_preserves_json_and_response_byte_limit() {
        let request = json!({ "queries": [{
            "package": { "ecosystem": "npm", "name": "public-package" },
            "version": "1.2.3"
        }] });
        for (body, accepted) in [
            (r#"{"results":[{}]}"#.to_owned(), true),
            (
                " ".repeat(MAX_RESPONSE + 1 - r#"{"results":[{}]}"#.len()) + r#"{"results":[{}]}"#,
                false,
            ),
        ] {
            let (endpoint, server) = serve_osv_once(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ));
            let result = send_batch(&osv_agent(), &endpoint, &request, Duration::from_secs(5));
            assert_eq!(result.is_ok(), accepted);
            if accepted {
                assert_eq!(result.unwrap(), json!({ "results": [{}] }));
            }
            assert_eq!(server.join().unwrap(), request);
        }
    }

    #[test]
    fn osv_redirects_are_rejected_without_forwarding_package_data() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let (endpoint, server) = serve_osv_once(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        ));
        let request = json!({ "queries": [] });
        assert!(send_batch(&osv_agent(), &endpoint, &request, Duration::from_secs(5),).is_err());
        assert_eq!(server.join().unwrap(), request);
        assert!(matches!(
            target.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    fn npm(lock: Value) -> (Packages, DependencyReport) {
        let mut packages = Packages::new();
        let mut report = DependencyReport::new();
        parse_npm(
            &serde_json::to_vec(&lock).unwrap(),
            "package-lock.json",
            &mut packages,
            &mut report,
        );
        (packages, report)
    }

    #[test]
    fn immutable_scope_only_includes_plugin_and_ancestors() {
        assert_eq!(
            ancestor_directories(Some("plugins/example")),
            Some(vec!["plugins/example".into(), "plugins".into(), "".into()])
        );
        for invalid in ["../outside", "/absolute", "a/./b", "a//b", "a\\b", "a\nb"] {
            assert!(ancestor_directories(Some(invalid)).is_none());
        }
    }

    #[test]
    fn cargo_queries_only_known_public_origins() {
        let mut packages = Packages::new();
        let mut report = DependencyReport::new();
        parse_cargo(
            br#"version = 4
[[package]]
name = "public-crate"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name = "secret-internal-name"
version = "2.0.0"
source = "registry+https://private.example/index"
[[package]]
name = "workspace-project"
version = "0.1.0"
"#,
            "Cargo.lock",
            &mut packages,
            &mut report,
        );
        assert_eq!(packages.len(), 1);
        assert_eq!(packages.keys().next().unwrap().ecosystem, "crates.io");
        assert!(!report.complete);
        assert!(!serde_json::to_string(&report)
            .unwrap()
            .contains("secret-internal-name"));
    }

    #[test]
    fn npm_v1_checks_nested_and_scoped_packages() {
        let (packages, report) = npm(json!({ "lockfileVersion": 1, "dependencies": {
            "outer": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/outer/-/outer-1.0.0.tgz", "dependencies": {
                "@scope/inner": { "version": "2.0.0", "resolved": "https://registry.npmjs.org/@scope/inner/-/inner-2.0.0.tgz" }
            }}
        }}));
        assert!(report.complete);
        assert_eq!(packages.len(), 2);
        assert!(packages
            .keys()
            .any(|package| package.name == "@scope/inner"));
    }

    #[test]
    fn npm_v2_v3_support_nested_scopes_and_aliases() {
        for version in [2, 3] {
            let (packages, report) = npm(json!({ "lockfileVersion": version, "packages": {
                "": { "name": "private-root", "version": "1.0.0", "private": true },
                "node_modules/@scope/pkg": { "version": "1.2.3", "resolved": "https://registry.npmjs.org/@scope/pkg/-/pkg-1.2.3.tgz" },
                "node_modules/outer/node_modules/alias": { "name": "real-package", "version": "2.0.0", "resolved": "https://registry.npmjs.org/real-package/-/real-package-2.0.0.tgz" }
            }}));
            assert!(report.complete);
            assert_eq!(packages.len(), 2);
            assert!(packages
                .keys()
                .any(|package| package.name == "real-package"));
        }
    }

    #[test]
    fn private_git_missing_and_deceptive_npm_origins_never_leave_machine() {
        let (packages, mut report) = npm(json!({ "lockfileVersion": 3, "packages": {
            "node_modules/secret-a": { "version": "1.0.0", "resolved": "https://registry.npmjs.org.evil.test/a" },
            "node_modules/secret-b": { "version": "1.0.0", "resolved": "git+https://example.test/a" },
            "node_modules/secret-c": { "version": "1.0.0" },
            "node_modules/secret-d": { "version": "1.0.0", "private": true, "resolved": "https://registry.npmjs.org/d" },
            "node_modules/secret-e": { "version": "1.0.0", "resolved": "https://token@registry.npmjs.org/e" },
            "node_modules/secret-f": { "version": "1.0.0", "resolved": "https://registry.npmjs.org/f?token=secret" }
        }}));
        query_packages(&packages, &mut report, Instant::now(), |_, _| {
            panic!("private names must not be queried")
        });
        assert!(!report.complete);
        assert_eq!(report.checked, 0);
        assert!(!serde_json::to_string(&report).unwrap().contains("secret-"));
    }

    #[test]
    fn malformed_and_unsupported_lockfiles_are_incomplete() {
        for lock in [
            json!({}),
            json!({ "lockfileVersion": 99 }),
            json!({ "lockfileVersion": 3, "packages": [] }),
        ] {
            assert!(!npm(lock).1.complete);
        }
        let mut report = DependencyReport::new();
        parse_cargo(
            b"not valid toml",
            "Cargo.lock",
            &mut Packages::new(),
            &mut report,
        );
        assert!(!report.complete);
    }

    fn public_packages() -> Packages {
        npm(json!({ "lockfileVersion": 3, "packages": {
            "node_modules/pkg": { "version": "1.2.3", "resolved": "https://registry.npmjs.org/pkg/-/pkg-1.2.3.tgz" }
        }})).0
    }

    #[test]
    fn advisory_findings_are_stable_high_and_contain_no_remote_text() {
        let packages = public_packages();
        let mut report = DependencyReport::new();
        query_packages(&packages, &mut report, Instant::now(), |request, _| {
            assert_eq!(
                *request,
                json!({ "queries": [{ "package": { "ecosystem": "npm", "name": "pkg" }, "version": "1.2.3" }] })
            );
            Ok(json!({ "results": [{ "vulns": [
                { "id": "GHSA-1234-5678-9012", "summary": "remote untrusted text\u{001b}" },
                { "id": "WITHDRAWN-1", "withdrawn": "2026-01-01T00:00:00Z" }
            ] }] }))
        });
        assert!(report.complete);
        assert_eq!(report.checked, 1);
        assert_eq!(report.findings.len(), 1);
        let finding = &report.findings[0];
        assert!(matches!(finding.severity, Severity::High));
        assert!(finding.is_new);
        assert!(finding
            .message
            .contains("https://osv.dev/vulnerability/GHSA-1234-5678-9012"));
        assert!(!finding.message.contains("remote untrusted"));
        let (package, paths) = packages.iter().next().unwrap();
        assert_eq!(
            finding.id,
            vulnerability_finding(package, paths.iter().next().unwrap(), "GHSA-1234-5678-9012").id
        );
    }

    #[test]
    fn pagination_failures_and_invalid_results_never_report_clean() {
        let packages = public_packages();
        for response in [
            Err(()),
            Ok(json!({})),
            Ok(json!({ "results": [] })),
            Ok(json!({ "results": [null] })),
            Ok(
                json!({ "results": [{ "next_page_token": "more", "vulns": [{ "id": "GHSA-first" }] }] }),
            ),
            Ok(json!({ "results": [{ "vulns": [{ "id": "../../untrusted\n" }] }] })),
            Ok(json!({ "results": [{ "vulns": null }] })),
        ] {
            let mut report = DependencyReport::new();
            let mut calls = 0;
            query_packages(&packages, &mut report, Instant::now(), |_, _| {
                calls += 1;
                response.clone()
            });
            assert!(!report.complete);
            assert_eq!(report.checked, 0);
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn complete_empty_osv_results_count_distinct_versions() {
        let mut packages = public_packages();
        let package = packages.keys().next().unwrap().clone();
        packages
            .get_mut(&package)
            .unwrap()
            .insert("workspace/package-lock.json".into());
        let mut report = DependencyReport::new();
        query_packages(&packages, &mut report, Instant::now(), |_, _| {
            Ok(json!({ "results": [{}] }))
        });
        assert!(report.complete);
        assert_eq!(report.checked, 1);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn batches_preserve_completed_counts_after_network_failure() {
        let mut packages = Packages::new();
        let mut report = DependencyReport::new();
        for n in 0..BATCH_SIZE + 1 {
            add_package(
                "npm",
                &format!("pkg-{n}"),
                "1.0.0",
                "package-lock.json",
                &mut packages,
                &mut report,
            );
        }
        let mut calls = 0;
        query_packages(&packages, &mut report, Instant::now(), |request, _| {
            calls += 1;
            let queries = request["queries"].as_array().unwrap();
            assert!(queries.len() <= BATCH_SIZE);
            if calls == 1 {
                Ok(json!({ "results": vec![json!({}); queries.len()] }))
            } else {
                Err(())
            }
        });
        assert_eq!(calls, 2);
        assert_eq!(report.checked, BATCH_SIZE);
        assert!(!report.complete);
    }

    #[test]
    fn elapsed_scan_never_sends_a_request() {
        let mut report = DependencyReport::new();
        query_packages(
            &public_packages(),
            &mut report,
            Instant::now() - SCAN_TIME,
            |_, _| panic!("deadline exceeded"),
        );
        assert!(!report.complete);
        assert_eq!(report.checked, 0);
    }
}
