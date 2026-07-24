use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const DEFAULT_LEDGER: &str = "target/sisyphus-functionality-ledger.tsv";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Severity {
    Error,
    Warning,
    Information,
}

impl Severity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Finding {
    severity: Severity,
    rule: &'static str,
    path: PathBuf,
    line: usize,
    detail: String,
}

#[derive(Clone, Debug)]
struct ModuleRecord {
    crate_name: String,
    module_name: String,
    declaration_path: PathBuf,
    source_path: Option<PathBuf>,
    external_reference_count: usize,
    tests: usize,
    public_items: usize,
    findings: usize,
}

#[derive(Clone, Debug)]
struct Configuration {
    root: PathBuf,
    ledger: PathBuf,
    baseline: Option<PathBuf>,
    write_baseline: Option<PathBuf>,
    deny_warnings: bool,
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("reality-gate: {error}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<i32, String> {
    let configuration = parse_arguments()?;
    let rust_files = collect_rust_files(&configuration.root)?;
    if rust_files.is_empty() {
        return Err("no Rust sources found".into());
    }

    let sources = load_sources(&rust_files)?;
    let mut findings = scan_sources(&configuration.root, &sources);
    let mut modules = discover_exported_modules(&configuration.root, &sources, &mut findings);

    count_references(&sources, &mut modules);
    attach_module_metrics(&sources, &findings, &mut modules);

    findings.sort_by(|left, right| {
        (
            left.severity,
            &left.path,
            left.line,
            left.rule,
            &left.detail,
        )
            .cmp(&(
                right.severity,
                &right.path,
                right.line,
                right.rule,
                &right.detail,
            ))
    });

    write_ledger(
        &configuration.root,
        &configuration.ledger,
        &modules,
        &findings,
    )?;

    if let Some(path) = &configuration.write_baseline {
        write_baseline(path, &findings)?;
    }

    let baseline = load_baseline(configuration.baseline.as_deref())?;
    let novel = findings
        .iter()
        .filter(|finding| !baseline.contains(&finding_fingerprint(finding)))
        .cloned()
        .collect::<Vec<_>>();

    print_summary(&configuration, &modules, &findings, &novel)?;

    if configuration.write_baseline.is_some() {
        return Ok(0);
    }

    let errors = novel
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count();
    let warnings = novel
        .iter()
        .filter(|finding| finding.severity == Severity::Warning)
        .count();

    if errors != 0 || (configuration.deny_warnings && warnings != 0) {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn parse_arguments() -> Result<Configuration, String> {
    let mut root = PathBuf::from(".");
    let mut ledger = PathBuf::from(DEFAULT_LEDGER);
    let mut baseline = None;
    let mut write_baseline = None;
    let mut deny_warnings = false;

    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                root = PathBuf::from(arguments.next().ok_or("--root requires a path")?);
            }
            "--ledger" => {
                ledger = PathBuf::from(arguments.next().ok_or("--ledger requires a path")?);
            }
            "--baseline" => {
                baseline = Some(PathBuf::from(
                    arguments.next().ok_or("--baseline requires a path")?,
                ));
            }
            "--write-baseline" => {
                write_baseline = Some(PathBuf::from(
                    arguments.next().ok_or("--write-baseline requires a path")?,
                ));
            }
            "--deny-warnings" => deny_warnings = true,
            "--help" | "-h" => {
                println!(
                    "usage: sisyphus-reality-gate \
                     [--root PATH] [--ledger PATH] \
                     [--baseline PATH] [--write-baseline PATH] \
                     [--deny-warnings]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let root = fs::canonicalize(&root)
        .map_err(|error| format!("cannot open {}: {error}", root.display()))?;
    let ledger = if ledger.is_absolute() {
        ledger
    } else {
        root.join(ledger)
    };

    let resolve = |path: Option<PathBuf>, root_ref: &Path| {
        path.map(|value| {
            if value.is_absolute() {
                value
            } else {
                root_ref.join(value)
            }
        })
    };

    Ok(Configuration {
        baseline: resolve(baseline, &root),
        write_baseline: resolve(write_baseline, &root),
        root,
        ledger,
        deny_warnings,
    })
}

fn collect_rust_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;

        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let name = entry.file_name();

            if path.is_dir() {
                if matches!(name.to_str(), Some(".git" | "target" | ".idea" | ".vscode"))
                    || path.ends_with("tools/reality-gate")
                {
                    continue;
                }
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn load_sources(files: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>, String> {
    let mut sources = BTreeMap::new();
    for path in files {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        sources.insert(path.clone(), text);
    }
    Ok(sources)
}

fn scan_sources(root: &Path, sources: &BTreeMap<PathBuf, String>) -> Vec<Finding> {
    let mut findings = Vec::new();

    for (path, source) in sources {
        let relative = relative_path(root, path);
        let production = production_prefix(source);

        if production.lines().any(|line| {
            let compact = line.split_whitespace().collect::<String>();
            (compact.starts_with("#![allow(") || compact.starts_with("#[allow("))
                && (compact.contains("dead_code") || compact.contains("unused"))
        }) {
            findings.push(Finding {
                severity: Severity::Error,
                rule: "module-dead-code-suppression",
                path: relative.clone(),
                line: production
                    .lines()
                    .position(|line| line.contains("dead_code"))
                    .map(|index| index + 1)
                    .unwrap_or(1),
                detail: "production dead/unused-code suppression hides disconnected code".into(),
            });
        }

        for (needle, severity, rule, detail) in [
            (
                "STUBS FOR",
                Severity::Error,
                "explicit-facade-marker",
                "source explicitly declares local subsystem stubs",
            ),
            (
                "Pretend ",
                Severity::Error,
                "pretend-behavior",
                "source describes a simulated success path as real behavior",
            ),
            (
                "pretend ",
                Severity::Error,
                "pretend-behavior",
                "source describes a simulated success path as real behavior",
            ),
            (
                "todo!(",
                Severity::Error,
                "unfinished-macro",
                "todo! is reachable in production source",
            ),
            (
                "unimplemented!(",
                Severity::Error,
                "unfinished-macro",
                "unimplemented! is reachable in production source",
            ),
            (
                "placeholder",
                Severity::Warning,
                "placeholder-language",
                "placeholder wording requires a reviewed, scoped exemption",
            ),
            (
                "mock of",
                Severity::Error,
                "mock-production-path",
                "mock implementation is present in production source",
            ),
        ] {
            for line in find_lines(production, needle) {
                findings.push(Finding {
                    severity,
                    rule,
                    path: relative.clone(),
                    line,
                    detail: detail.into(),
                });
            }
        }

        scan_suspicious_functions(&relative, production, &mut findings);

        if relative.ends_with("src/main.rs")
            && production.contains("loop {")
            && production.contains("yield_now()")
            && !production.contains("render_dirty(")
            && !production.contains("dispatch(")
            && !production.contains("run(")
        {
            findings.push(Finding {
                severity: Severity::Warning,
                rule: "idle-entrypoint",
                path: relative.clone(),
                line: find_lines(production, "loop {")
                    .into_iter()
                    .next()
                    .unwrap_or(1),
                detail: "entry point appears to yield forever without executing a service loop"
                    .into(),
            });
        }
    }

    findings
}

fn scan_suspicious_functions(path: &Path, source: &str, findings: &mut Vec<Finding>) {
    let bytes = source.as_bytes();
    let mut cursor = 0_usize;

    while cursor + 3 < bytes.len() {
        let Some(offset) = source[cursor..].find("fn ") else {
            break;
        };
        let start = cursor + offset;
        if start != 0 {
            let previous = bytes[start - 1];
            if previous.is_ascii_alphanumeric() || previous == b'_' {
                cursor = start + 3;
                continue;
            }
        }

        let name_start = start + 3;
        let name_end = source[name_start..]
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .map(|value| name_start + value)
            .unwrap_or(bytes.len());
        let name = &source[name_start..name_end];

        let Some(open_relative) = source[name_end..].find('{') else {
            break;
        };
        let open = name_end + open_relative;
        let Some(close) = matching_brace(source, open) else {
            cursor = open + 1;
            continue;
        };

        let body = source[open + 1..close].trim();
        let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");

        let exempt = matches!(
            name,
            "new"
                | "default"
                | "empty"
                | "zeroed"
                | "len"
                | "is_empty"
                | "root"
                | "generation"
                | "count"
                | "capacity"
        );

        let suspicious = matches!(
            normalized.as_str(),
            "" | "0" | "false" | "None" | "Ok(())" | "Some(0)" | "Err(())" | "()"
        );

        if suspicious && !exempt {
            findings.push(Finding {
                severity: Severity::Warning,
                rule: "constant-or-empty-function",
                path: path.to_path_buf(),
                line: line_number(source, start),
                detail: format!("function `{name}` has a constant or empty body: `{normalized}`"),
            });
        }

        cursor = close + 1;
    }
}

fn matching_brace(source: &str, opening: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0_i32;
    let mut index = opening;
    let mut in_string = false;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment_depth = 0_i32;

    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();

        if line_comment {
            if byte == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment_depth != 0 {
            if byte == b'/' && next == Some(b'*') {
                block_comment_depth += 1;
                index += 2;
                continue;
            }
            if byte == b'*' && next == Some(b'/') {
                block_comment_depth -= 1;
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if byte == b'/' && next == Some(b'/') {
            line_comment = true;
            index += 2;
            continue;
        }
        if byte == b'/' && next == Some(b'*') {
            block_comment_depth = 1;
            index += 2;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }

        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }

    None
}

fn discover_exported_modules(
    root: &Path,
    sources: &BTreeMap<PathBuf, String>,
    findings: &mut Vec<Finding>,
) -> Vec<ModuleRecord> {
    let mut modules = Vec::new();

    for (path, source) in sources {
        if path.file_name().and_then(|name| name.to_str()) != Some("lib.rs") {
            continue;
        }

        let crate_name = path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();

        let source_directory = path.parent().unwrap_or(root);

        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            let declaration = trimmed
                .strip_prefix("pub mod ")
                .and_then(|value| value.strip_suffix(';'));

            let Some(module_name) = declaration else {
                continue;
            };
            if !module_name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            {
                continue;
            }

            let flat = source_directory.join(format!("{module_name}.rs"));
            let nested = source_directory.join(module_name).join("mod.rs");
            let source_path = if flat.is_file() {
                Some(flat)
            } else if nested.is_file() {
                Some(nested)
            } else {
                findings.push(Finding {
                    severity: Severity::Error,
                    rule: "missing-module-source",
                    path: relative_path(root, path),
                    line: index + 1,
                    detail: format!("exported module `{module_name}` has no source file"),
                });
                None
            };

            modules.push(ModuleRecord {
                crate_name: crate_name.clone(),
                module_name: module_name.to_string(),
                declaration_path: relative_path(root, path),
                source_path: source_path.as_ref().map(|value| relative_path(root, value)),
                external_reference_count: 0,
                tests: 0,
                public_items: 0,
                findings: 0,
            });
        }
    }

    modules
}

fn count_references(sources: &BTreeMap<PathBuf, String>, modules: &mut [ModuleRecord]) {
    for module in modules {
        let token = format!("{}::", module.module_name);
        let mut count = 0_usize;

        for (path, source) in sources {
            if module
                .source_path
                .as_ref()
                .is_some_and(|module_path| path.ends_with(module_path))
            {
                continue;
            }
            if path.ends_with(&module.declaration_path) {
                continue;
            }
            count = count.saturating_add(source.matches(&token).count());
        }

        module.external_reference_count = count;
    }
}

fn attach_module_metrics(
    sources: &BTreeMap<PathBuf, String>,
    findings: &[Finding],
    modules: &mut [ModuleRecord],
) {
    for module in modules {
        let Some(source_path) = &module.source_path else {
            continue;
        };

        if let Some((_, source)) = sources.iter().find(|(path, _)| path.ends_with(source_path)) {
            module.tests = source.matches("#[test]").count();
            module.public_items = source.matches("pub fn ").count()
                + source.matches("pub struct ").count()
                + source.matches("pub enum ").count()
                + source.matches("pub trait ").count();
        }

        module.findings = findings
            .iter()
            .filter(|finding| &finding.path == source_path)
            .count();
    }
}

fn write_ledger(
    root: &Path,
    path: &Path,
    modules: &[ModuleRecord],
    findings: &[Finding],
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    let mut output = fs::File::create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;

    writeln!(
        output,
        "record\tcrate\tmodule\tpath\treferences\ttests\tpublic_items\tfindings\tseverity\trule\tline\tdetail"
    )
    .map_err(|error| error.to_string())?;

    for module in modules {
        writeln!(
            output,
            "module\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\t\t\t",
            escape(&module.crate_name),
            escape(&module.module_name),
            escape(
                &module
                    .source_path
                    .as_ref()
                    .map(|value| value.display().to_string())
                    .unwrap_or_else(|| "<missing>".into())
            ),
            module.external_reference_count,
            module.tests,
            module.public_items,
            module.findings,
        )
        .map_err(|error| error.to_string())?;
    }

    for finding in findings {
        writeln!(
            output,
            "finding\t\t\t{}\t\t\t\t\t{}\t{}\t{}\t{}",
            escape(&finding.path.display().to_string()),
            finding.severity.as_str(),
            finding.rule,
            finding.line,
            escape(&finding.detail),
        )
        .map_err(|error| error.to_string())?;
    }

    let _ = root;
    Ok(())
}

fn print_summary(
    configuration: &Configuration,
    modules: &[ModuleRecord],
    findings: &[Finding],
    novel: &[Finding],
) -> Result<(), String> {
    let errors = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Error)
        .count();
    let warnings = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Warning)
        .count();
    let information = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Information)
        .count();
    let orphaned = modules
        .iter()
        .filter(|module| module.external_reference_count == 0)
        .count();

    println!("Sisyphus Functional Reality Gate");
    println!("root={}", configuration.root.display());
    println!("exported_modules={}", modules.len());
    println!("orphaned_modules={orphaned}");
    println!("errors={errors}");
    println!("warnings={warnings}");
    println!("information={information}");
    println!("novel_findings={}", novel.len());
    println!("ledger={}", configuration.ledger.display());
    if let Some(path) = &configuration.baseline {
        println!("baseline={}", path.display());
    }

    for finding in novel.iter().take(200) {
        println!(
            "{}:{}: {} [{}] {}",
            finding.path.display(),
            finding.line,
            finding.severity.as_str(),
            finding.rule,
            finding.detail,
        );
    }

    if novel.len() > 200 {
        println!(
            "... {} additional novel findings in ledger",
            novel.len() - 200
        );
    }

    io::stdout().flush().map_err(|error| error.to_string())
}

fn production_prefix(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

fn find_lines(source: &str, needle: &str) -> Vec<usize> {
    source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| line.contains(needle).then_some(index + 1))
        .collect()
}

fn line_number(source: &str, byte_offset: usize) -> usize {
    source[..byte_offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn finding_fingerprint(finding: &Finding) -> String {
    format!(
        "{}\t{}\t{}",
        finding.rule,
        finding.path.display(),
        finding.detail,
    )
}

fn load_baseline(path: Option<&Path>) -> Result<BTreeSet<String>, String> {
    let Some(path) = path else {
        return Ok(BTreeSet::new());
    };

    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read baseline {}: {error}", path.display()))?;
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn write_baseline(path: &Path, findings: &[Finding]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    let mut fingerprints = findings.iter().map(finding_fingerprint).collect::<Vec<_>>();
    fingerprints.sort();
    fingerprints.dedup();

    let mut output = fs::File::create(path)
        .map_err(|error| format!("cannot create baseline {}: {error}", path.display()))?;
    writeln!(
        output,
        "# Existing functionality debt. Delete entries as modules are repaired."
    )
    .map_err(|error| error.to_string())?;
    for fingerprint in fingerprints {
        writeln!(output, "{fingerprint}").map_err(|error| error.to_string())?;
    }
    Ok(())
}
