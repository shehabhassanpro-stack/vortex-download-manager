//! Vortex Architecture Fitness Function (`tools/arch-check` — `P0-T08`).
//!
//! Enforces at CI time:
//! 1. Strict 5-Level Topological Crate DAG across `[dependencies]`, `[build-dependencies]`,
//!    and all `[target.*.dependencies]` tables (`Vortex-Architecture-Design.md` §6.4).
//! 2. Zero I/O or async dependencies inside `vortex-domain` (`ADR-003`).
//! 3. Thin-binary isolation for `apps/bridge` and `apps/cli` (never pulling `vortex-engine`,
//!    SQLite, or enabling the `server` feature on `vortex-ipc`).
//! 4. Mandatory `[lints] workspace = true` inheritance on every workspace member and preservation
//!    of Zero-Panic `deny` lints in `[workspace.lints.clippy]` (`Engineering-Standards.md` §2.1).
//! 5. `unsafe` prohibition in forbidden crates and mandatory `// SAFETY:` comments elsewhere (`§2.2`).
//! 6. Preservation of `panic = "unwind"` in `[profile.release]` (`ADR-004` / `ADR-015`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Workspace dependency layer classification (5-level strict DAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    /// Level 0: `vortex-error` — zero internal dependencies.
    Level0Error,
    /// Level 1A: `vortex-observability` — may only depend on Level 0.
    Level1Observability,
    /// Level 1B: `vortex-domain` — pure Rust, zero I/O, may only depend on Level 0.
    Level1Domain,
    /// Level 2: `vortex-application` — may only depend on Levels 0 & 1.
    Level2Application,
    /// Level 3 Transport: `vortex-ipc` — client deps Level 0/1; optional server may use Level 2.
    Level3Ipc,
    /// Level 3 Engine & Adapters — may only depend on Levels 0, 1, 2.
    Level3Impl,
    /// Level 4 Thin Apps: `vortex-bridge`, `vortex-cli` — only Level 0, 1A, and `vortex-ipc` (`client`).
    Level4ThinApp,
    /// Level 4 Composition Root: `vortex-desktop` — may depend on Levels 0..3.
    Level4CompositionRoot,
    /// Engineering/testing tools outside the runtime DAG.
    Tool,
}

/// External crates strictly banned in `vortex-domain` (pure domain invariant).
const DOMAIN_BANNED_EXTERNAL_DEPS: &[&str] = &[
    "tokio",
    "hyper",
    "hyper-util",
    "reqwest",
    "quinn",
    "h3",
    "hickory-resolver",
    "rusqlite",
    "sqlx",
    "rustls",
    "socket2",
    "windows",
    "windows-sys",
    "tauri",
    "wasmtime",
    "prost",
    "axum",
    "suppaftp",
    "russh",
    "librqbit",
];

/// Standard library I/O modules banned in `vortex-domain` production code.
const DOMAIN_BANNED_STD_MODULES: &[&str] = &["std::fs", "std::net", "std::process", "tokio::"];

/// Crates where `unsafe` code is strictly prohibited (`Engineering-Standards.md` §2.2).
const UNSAFE_BANNED_CRATES: &[&str] = &[
    "vortex-error",
    "vortex-domain",
    "vortex-application",
    "vortex-engine",
    "vortex-ipc",
    "vortex-media",
    "vortex-proto-ftp",
];

/// Crates forbidden from being depended on by `vortex-ipc` or thin apps (`bridge`/`cli`).
const HEAVY_ENGINE_DEPS: &[&str] = &[
    "vortex-engine",
    "vortex-storage",
    "vortex-io",
    "vortex-net",
    "vortex-platform",
    "vortex-media",
    "vortex-proto-ftp",
    "vortex-plugin-host",
    "rusqlite",
    "tauri",
    "wasmtime",
];

/// Mandatory Zero-Panic Clippy lints that must remain `"deny"` in `[workspace.lints.clippy]`.
const REQUIRED_DENY_CLIPPY_LINTS: &[&str] = &[
    "unwrap_used",
    "expect_used",
    "panic",
    "indexing_slicing",
    "await_holding_lock",
];

/// Describes a single architectural rule violation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    rule: &'static str,
    location: String,
    message: String,
}

fn classify_crate(name: &str) -> Option<Layer> {
    match name {
        "vortex-error" => Some(Layer::Level0Error),
        "vortex-observability" => Some(Layer::Level1Observability),
        "vortex-domain" => Some(Layer::Level1Domain),
        "vortex-application" => Some(Layer::Level2Application),
        "vortex-ipc" => Some(Layer::Level3Ipc),
        "vortex-engine" | "vortex-net" | "vortex-storage" | "vortex-io" | "vortex-platform"
        | "vortex-media" | "vortex-proto-ftp" | "vortex-plugin-host" => Some(Layer::Level3Impl),
        "vortex-bridge" | "vortex-cli" => Some(Layer::Level4ThinApp),
        "vortex-desktop" => Some(Layer::Level4CompositionRoot),
        "vortex-arch-check" | "vortex-test-lab" | "vortex-bench" => Some(Layer::Tool),
        _ => None,
    }
}

fn is_internal_dep_allowed(from: Layer, to: Layer, to_name: &str) -> bool {
    match from {
        Layer::Level0Error => false,
        Layer::Level1Observability | Layer::Level1Domain => to == Layer::Level0Error,
        Layer::Level2Application => matches!(
            to,
            Layer::Level0Error | Layer::Level1Observability | Layer::Level1Domain
        ),
        Layer::Level3Ipc => matches!(
            to,
            Layer::Level0Error | Layer::Level1Observability | Layer::Level2Application
        ),
        Layer::Level3Impl => matches!(
            to,
            Layer::Level0Error
                | Layer::Level1Observability
                | Layer::Level1Domain
                | Layer::Level2Application
        ),
        Layer::Level4ThinApp => {
            matches!(to, Layer::Level0Error | Layer::Level1Observability) || to_name == "vortex-ipc"
        }
        Layer::Level4CompositionRoot => !matches!(to, Layer::Level4ThinApp | Layer::Tool),
        Layer::Tool => true,
    }
}

/// Collects dependency keys across `[dependencies]`, `[build-dependencies]`, and all `[target.*.dependencies]`.
fn collect_all_non_dev_deps(doc: &toml::Value) -> BTreeMap<String, toml::Value> {
    let mut deps = BTreeMap::new();
    for section in ["dependencies", "build-dependencies"] {
        if let Some(table) = doc.get(section).and_then(toml::Value::as_table) {
            for (k, v) in table {
                deps.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for (_, target_cfg) in targets {
            for section in ["dependencies", "build-dependencies"] {
                if let Some(table) = target_cfg.get(section).and_then(toml::Value::as_table) {
                    for (k, v) in table {
                        deps.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    deps
}

fn check_root_manifest_invariants(root_manifest: &toml::Value, violations: &mut Vec<Violation>) {
    let panic_setting = root_manifest
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.get("panic"))
        .and_then(toml::Value::as_str);

    if panic_setting != Some("unwind") {
        violations.push(Violation {
            rule: "RELEASE_PANIC_UNWIND",
            location: "Cargo.toml".to_owned(),
            message: format!(
                "[profile.release].panic must be \"unwind\" to preserve Actor Bulkhead isolation (found: {panic_setting:?})"
            ),
        });
    }

    let clippy_lints = root_manifest
        .get("workspace")
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.get("clippy"))
        .and_then(toml::Value::as_table);

    for &lint_name in REQUIRED_DENY_CLIPPY_LINTS {
        let level = clippy_lints.and_then(|t| t.get(lint_name)).and_then(|v| {
            v.as_str()
                .or_else(|| v.get("level").and_then(toml::Value::as_str))
        });
        if level != Some("deny") {
            violations.push(Violation {
                rule: "ZERO_PANIC_LINT_TAMPERED",
                location: "Cargo.toml".to_owned(),
                message: format!(
                    "[workspace.lints.clippy].{lint_name} must be \"deny\" (found: {level:?})"
                ),
            });
        }
    }
}

fn check_member_lints_inheritance(
    crate_name: &str,
    manifest_rel: &str,
    manifest: &toml::Value,
    violations: &mut Vec<Violation>,
) {
    let inherits_workspace_lints = manifest
        .get("lints")
        .and_then(|l| l.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true);

    if !inherits_workspace_lints {
        violations.push(Violation {
            rule: "MISSING_WORKSPACE_LINTS",
            location: manifest_rel.to_owned(),
            message: format!("`{crate_name}` must declare `[lints] workspace = true`"),
        });
    }
}

fn check_crate_dependencies(
    crate_name: &str,
    manifest_rel: &str,
    manifest: &toml::Value,
    workspace_crates: &BTreeMap<String, Layer>,
    violations: &mut Vec<Violation>,
) {
    check_member_lints_inheritance(crate_name, manifest_rel, manifest, violations);

    let Some(from_layer) = classify_crate(crate_name) else {
        violations.push(Violation {
            rule: "UNKNOWN_CRATE",
            location: manifest_rel.to_owned(),
            message: format!(
                "Workspace crate `{crate_name}` is not classified in tools/arch-check 5-level DAG"
            ),
        });
        return;
    };

    if from_layer == Layer::Tool {
        return;
    }

    let runtime_deps = collect_all_non_dev_deps(manifest);

    for (dep, dep_spec) in &runtime_deps {
        if let Some(&to_layer) = workspace_crates.get(dep)
            && !is_internal_dep_allowed(from_layer, to_layer, dep)
        {
            violations.push(Violation {
                rule: "DAG_VIOLATION",
                location: manifest_rel.to_owned(),
                message: format!(
                    "`{crate_name}` ({from_layer:?}) must NOT depend on `{dep}` ({to_layer:?})"
                ),
            });
        }

        if from_layer == Layer::Level1Domain && DOMAIN_BANNED_EXTERNAL_DEPS.contains(&dep.as_str())
        {
            violations.push(Violation {
                rule: "DOMAIN_IO_BAN",
                location: manifest_rel.to_owned(),
                message: format!(
                    "`vortex-domain` must have zero I/O/async dependencies; found banned crate `{dep}`"
                ),
            });
        }

        if matches!(from_layer, Layer::Level3Ipc | Layer::Level4ThinApp)
            && HEAVY_ENGINE_DEPS.contains(&dep.as_str())
        {
            violations.push(Violation {
                rule: "THIN_BINARY_ISOLATION",
                location: manifest_rel.to_owned(),
                message: format!(
                    "`{crate_name}` must remain lightweight and never depend on `{dep}`"
                ),
            });
        }

        // Thin apps (bridge / cli) must only use `vortex-ipc` `client`, never `server`
        if from_layer == Layer::Level4ThinApp && dep == "vortex-ipc" {
            let enables_server = dep_spec
                .get("features")
                .and_then(toml::Value::as_array)
                .is_some_and(|arr| arr.iter().any(|f| f.as_str() == Some("server")));
            if enables_server {
                violations.push(Violation {
                    rule: "THIN_BINARY_ISOLATION",
                    location: manifest_rel.to_owned(),
                    message: format!(
                        "`{crate_name}` must not enable the `server` feature on `vortex-ipc`"
                    ),
                });
            }
        }
    }
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let is_target = path.file_name().is_some_and(|n| n == "target");
            if !is_target {
                collect_rs_files(&path, out)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn contains_unsafe_keyword(code_part: &str) -> bool {
    let bytes = code_part.as_bytes();
    let needle = b"unsafe";
    if bytes.len() < needle.len() {
        return false;
    }

    let mut idx = 0;
    while idx + needle.len() <= bytes.len() {
        if bytes.get(idx..idx + needle.len()) == Some(needle) {
            let before_ok = idx == 0
                || bytes
                    .get(idx - 1)
                    .is_some_and(|b| !b.is_ascii_alphanumeric() && *b != b'_');
            let after_ok = idx + needle.len() == bytes.len()
                || bytes
                    .get(idx + needle.len())
                    .is_some_and(|b| !b.is_ascii_alphanumeric() && *b != b'_');
            if before_ok && after_ok {
                return true;
            }
        }
        idx += 1;
    }
    false
}

fn strip_comments_and_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            break;
        }
        if ch == '"' {
            in_string = true;
            out.push(' ');
            continue;
        }
        out.push(ch);
    }

    out
}

fn check_source_file(
    crate_name: &str,
    rel_path: &str,
    content: &str,
    violations: &mut Vec<Violation>,
) {
    let lines: Vec<&str> = content.lines().collect();
    let mut in_cfg_test = false;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[cfg(test)]") {
            in_cfg_test = true;
        }

        let code_only = strip_comments_and_string_literals(trimmed);

        if crate_name == "vortex-domain" && !in_cfg_test {
            for banned in DOMAIN_BANNED_STD_MODULES {
                if code_only.contains(banned) {
                    violations.push(Violation {
                        rule: "DOMAIN_IO_BAN",
                        location: format!("{rel_path}:{}", index + 1),
                        message: format!(
                            "`vortex-domain` production code must not reference `{banned}`"
                        ),
                    });
                }
            }
        }

        if code_only.contains("unsafe_code") || code_only.starts_with("#!") {
            continue;
        }

        if contains_unsafe_keyword(&code_only) {
            if UNSAFE_BANNED_CRATES.contains(&crate_name) {
                violations.push(Violation {
                    rule: "UNSAFE_BANNED",
                    location: format!("{rel_path}:{}", index + 1),
                    message: format!(
                        "`unsafe` is strictly prohibited in `{crate_name}` (Engineering-Standards.md §2.2)"
                    ),
                });
            } else {
                let has_safety_same_line = line.contains("// SAFETY:");
                let has_safety_prev_lines = (1..=3).any(|offset| {
                    index
                        .checked_sub(offset)
                        .and_then(|i| lines.get(i))
                        .is_some_and(|prev| prev.contains("// SAFETY:"))
                });
                if !has_safety_same_line && !has_safety_prev_lines {
                    violations.push(Violation {
                        rule: "UNSAFE_MISSING_SAFETY_COMMENT",
                        location: format!("{rel_path}:{}", index + 1),
                        message:
                            "Every `unsafe` usage must have an adjacent `// SAFETY:` justification"
                                .to_owned(),
                    });
                }
            }
        }
    }
}

fn find_workspace_root() -> io::Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)?;
            if text.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Could not locate Vortex workspace root Cargo.toml",
            ));
        }
    }
}

fn run_audit(workspace_root: &Path) -> Result<Vec<Violation>, String> {
    let root_toml_path = workspace_root.join("Cargo.toml");
    let root_text = fs::read_to_string(&root_toml_path)
        .map_err(|e| format!("failed to read root Cargo.toml: {e}"))?;
    let root_doc: toml::Value = root_text
        .parse()
        .map_err(|e| format!("failed to parse root Cargo.toml: {e}"))?;

    let mut violations = Vec::new();
    check_root_manifest_invariants(&root_doc, &mut violations);

    let members = root_doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "root Cargo.toml missing [workspace].members".to_owned())?;

    let mut member_manifests: Vec<(String, String, PathBuf, toml::Value)> = Vec::new();
    let mut workspace_crates: BTreeMap<String, Layer> = BTreeMap::new();

    for member in members {
        let Some(member_rel) = member.as_str() else {
            continue;
        };
        let member_dir = workspace_root.join(member_rel);
        let manifest_path = member_dir.join("Cargo.toml");
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("failed to read {member_rel}/Cargo.toml: {e}"))?;
        let manifest_doc: toml::Value = manifest_text
            .parse()
            .map_err(|e| format!("failed to parse {member_rel}/Cargo.toml: {e}"))?;
        let crate_name = manifest_doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{member_rel}/Cargo.toml missing [package].name"))?
            .to_owned();

        if let Some(layer) = classify_crate(&crate_name) {
            workspace_crates.insert(crate_name.clone(), layer);
        }
        member_manifests.push((
            crate_name,
            format!("{member_rel}/Cargo.toml"),
            member_dir,
            manifest_doc,
        ));
    }

    for (crate_name, manifest_rel, member_dir, manifest_doc) in &member_manifests {
        check_crate_dependencies(
            crate_name,
            manifest_rel,
            manifest_doc,
            &workspace_crates,
            &mut violations,
        );

        let src_dir = member_dir.join("src");
        let mut rs_files = Vec::new();
        collect_rs_files(&src_dir, &mut rs_files)
            .map_err(|e| format!("failed to scan {}: {e}", src_dir.display()))?;

        for rs_file in rs_files {
            let content = fs::read_to_string(&rs_file)
                .map_err(|e| format!("failed to read {}: {e}", rs_file.display()))?;
            let rel = rs_file
                .strip_prefix(workspace_root)
                .unwrap_or(&rs_file)
                .display()
                .to_string()
                .replace('\\', "/");
            check_source_file(crate_name, &rel, &content, &mut violations);
        }
    }

    Ok(violations)
}

fn main() -> ExitCode {
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    let root = match find_workspace_root() {
        Ok(path) => path,
        Err(err) => {
            let _ = writeln!(stderr, "arch-check error: {err}");
            return ExitCode::FAILURE;
        }
    };

    match run_audit(&root) {
        Ok(violations) if violations.is_empty() => {
            let _ = writeln!(
                stdout,
                "vortex-arch-check: OK (5-level crate DAG, target-deps, workspace lints, pure domain, unsafe policy, and release panic=unwind verified)"
            );
            ExitCode::SUCCESS
        }
        Ok(violations) => {
            let _ = writeln!(
                stderr,
                "vortex-arch-check: FAILED with {} violation(s):",
                violations.len()
            );
            for v in violations {
                let _ = writeln!(stderr, "  [{}] {} — {}", v.rule, v.location, v.message);
            }
            ExitCode::FAILURE
        }
        Err(err) => {
            let _ = writeln!(stderr, "vortex-arch-check internal error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn sample_workspace_map() -> BTreeMap<String, Layer> {
        BTreeMap::from([
            ("vortex-error".to_owned(), Layer::Level0Error),
            (
                "vortex-observability".to_owned(),
                Layer::Level1Observability,
            ),
            ("vortex-domain".to_owned(), Layer::Level1Domain),
            ("vortex-application".to_owned(), Layer::Level2Application),
            ("vortex-engine".to_owned(), Layer::Level3Impl),
            ("vortex-net".to_owned(), Layer::Level3Impl),
            ("vortex-storage".to_owned(), Layer::Level3Impl),
            ("vortex-ipc".to_owned(), Layer::Level3Ipc),
            ("vortex-bridge".to_owned(), Layer::Level4ThinApp),
            ("vortex-desktop".to_owned(), Layer::Level4CompositionRoot),
        ])
    }

    #[test]
    fn domain_cannot_depend_on_tokio_or_target_specific_windows_sys() {
        let map = sample_workspace_map();
        let manifest: toml::Value = r#"
            [lints]
            workspace = true

            [dependencies]
            vortex-error = { path = "../vortex-error" }
            vortex-application = { path = "../vortex-application" }

            [target.'cfg(windows)'.dependencies]
            windows-sys = "0.61"
        "#
        .parse()
        .unwrap();

        let mut violations = Vec::new();
        check_crate_dependencies(
            "vortex-domain",
            "crates/vortex-domain/Cargo.toml",
            &manifest,
            &map,
            &mut violations,
        );

        assert!(violations.iter().any(|v| v.rule == "DAG_VIOLATION"));
        assert!(violations.iter().any(|v| v.rule == "DOMAIN_IO_BAN"));
    }

    #[test]
    fn bridge_cannot_enable_ipc_server_or_omit_workspace_lints() {
        let map = sample_workspace_map();
        let manifest: toml::Value = r#"
            [dependencies]
            vortex-ipc = { path = "../../crates/vortex-ipc", features = ["server"] }
        "#
        .parse()
        .unwrap();

        let mut violations = Vec::new();
        check_crate_dependencies(
            "vortex-bridge",
            "apps/bridge/Cargo.toml",
            &manifest,
            &map,
            &mut violations,
        );

        assert!(
            violations
                .iter()
                .any(|v| v.rule == "MISSING_WORKSPACE_LINTS")
        );
        assert!(violations.iter().any(|v| v.rule == "THIN_BINARY_ISOLATION"));
    }

    #[test]
    fn unsafe_policy_ignores_string_literals_and_enforces_safety_comments() {
        let mut string_literal_ok = Vec::new();
        check_source_file(
            "vortex-domain",
            "crates/vortex-domain/src/lib.rs",
            "const MSG: &str = \"unsafe is just a word inside quotes\";",
            &mut string_literal_ok,
        );
        assert_eq!(string_literal_ok.len(), 0);

        let mut banned_violations = Vec::new();
        check_source_file(
            "vortex-domain",
            "crates/vortex-domain/src/lib.rs",
            "// SAFETY: not allowed in domain\nunsafe fn bad() {}",
            &mut banned_violations,
        );
        assert_eq!(banned_violations.len(), 1);
        assert_eq!(banned_violations[0].rule, "UNSAFE_BANNED");

        let mut justified = Vec::new();
        check_source_file(
            "vortex-io",
            "crates/vortex-io/src/win.rs",
            "// SAFETY: valid handle checked above\nunsafe { do_win32(); }",
            &mut justified,
        );
        assert_eq!(justified.len(), 0);
    }

    #[test]
    fn root_manifest_requires_unwind_and_zero_panic_clippy_denies() {
        let bad: toml::Value = r#"
            [profile.release]
            panic = "abort"

            [workspace.lints.clippy]
            unwrap_used = "warn"
        "#
        .parse()
        .unwrap();
        let mut violations = Vec::new();
        check_root_manifest_invariants(&bad, &mut violations);
        assert!(violations.iter().any(|v| v.rule == "RELEASE_PANIC_UNWIND"));
        assert!(
            violations
                .iter()
                .any(|v| v.rule == "ZERO_PANIC_LINT_TAMPERED")
        );
    }
}
