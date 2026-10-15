//! Bounded structural hints shared by desktop and CLI grep implementations.
//!
//! These are lexical declaration hints, not an AST or a claim that a match
//! belongs to the declaration (preprocessor/macro nesting is language-specific).
//! The caller retains the actual regex match and workspace permission policy.
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

const MAX_SCAN_BACK_LINES: usize = 250;
const MAX_DECLARATION_CHARS: usize = 180;

fn declaration_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(
        r"^\s*(?:(?:export|default|public|private|protected|static|final|override|open|internal|pub|unsafe|async|abstract|declare|inline|suspend)\s+)*(?:(?:pub\([^)]*\))\s*)?(?:(fn|def|class|struct|enum|trait|interface|function|impl|type|mod|object|namespace|module)\s+)([\p{L}_$][\p{L}\p{N}_$]*)"
    ).expect("valid static structural search regex"))
}

fn arrow_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(
        r"^\s*(?:(?:export|default|public|private|protected|static|const|let|var|async)\s+)*([\p{L}_$][\p{L}\p{N}_$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^;]*?\)|[\p{L}_$][\p{L}\p{N}_$]*)\s*=>"
    ).expect("valid static arrow declaration regex"))
}

fn has_supported_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).unwrap_or_default(),
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" |
        "go" | "java" | "kt" | "kts" | "swift" | "c" | "cc" | "cpp" |
        "h" | "hpp" | "cs" | "php"
    )
}

/// For a matching one-based line, return at most one nearby named declaration.
/// `distance` is an honest line displacement, NOT a semantic containment claim.
/// Unknown languages, binary data and matches without nearby declarations get
/// no invented symbols. Call only for real regex matches from bounded grep.
pub fn nearest_declaration(path: &Path, source: &str, line_number: usize) -> Option<Value> {
    if !has_supported_extension(path) || line_number == 0 {
        return None;
    }
    let lines: Vec<&str> = source.lines().take(line_number).collect();
    if lines.len() < line_number { return None; }
    let first = line_number.saturating_sub(MAX_SCAN_BACK_LINES + 1);
    for index in (first..line_number).rev() {
        let line = lines[index];
        if line.len() > 4096 { continue; }
        if let Some(captures) = declaration_pattern().captures(line) {
            let kind = captures.get(1)?.as_str();
            let name = captures.get(2)?.as_str();
            return Some(json!({
                "kind": kind,
                "name": name,
                "declaration_line": index + 1,
                "distance": line_number - (index + 1),
                "declaration": line.trim().chars().take(MAX_DECLARATION_CHARS).collect::<String>(),
                "confidence": "lexical_nearest_preceding"
            }));
        }
        if matches!(path.extension().and_then(|e| e.to_str()), Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")) {
            if let Some(captures) = arrow_pattern().captures(line) {
                return Some(json!({
                    "kind": "arrow_function",
                    "name": captures.get(1)?.as_str(),
                    "declaration_line": index + 1,
                    "distance": line_number - (index + 1),
                    "declaration": line.trim().chars().take(MAX_DECLARATION_CHARS).collect::<String>(),
                    "confidence": "lexical_nearest_preceding"
                }));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_function_offset_is_grounded_in_actual_lines() {
        let code = "use std::fmt;\npub fn parse() {\n let x = 1;\n process(x);\n}\n";
        let v = nearest_declaration(Path::new("src/main.rs"), code, 4).unwrap();
        assert_eq!(v["kind"], "fn");
        assert_eq!(v["name"], "parse");
        assert_eq!(v["declaration_line"], 2);
        assert_eq!(v["distance"], 2);
    }

    #[test]
    fn supports_ts_arrow_and_python_def_but_not_unrelated_markdown() {
        let arrow = "const fetchUser = async (id) => {\n return lookup(id);\n};";
        assert_eq!(nearest_declaration(Path::new("lib.ts"), arrow, 2).unwrap()["name"], "fetchUser");
        assert_eq!(nearest_declaration(Path::new("pkg/m.py"), "def process(value):\n  return value", 2).unwrap()["name"], "process");
        assert!(nearest_declaration(Path::new("README.md"), arrow, 2).is_none());
    }

    #[test]
    fn no_unbounded_scan_or_fake_symbol_context() {
        let many = format!("fn ancient() {{}}\n{}hit\n", "\n".repeat(300));
        assert!(nearest_declaration(Path::new("lib.rs"), &many, 302).is_none());
        assert!(nearest_declaration(Path::new("lib.rs"), "let x = 2;\n", 1).is_none());
        assert!(nearest_declaration(Path::new("lib.rs"), "", 1).is_none());
    }
}
