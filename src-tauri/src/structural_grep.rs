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

/// Conservative lexical region bounds. This is intentionally NOT an AST:
/// indentation closes Python suites and balanced delimiters close brace
/// languages. Strings/comments are skipped when counting braces. Unsupported
/// or unbalanced inputs fall back to an explicitly weaker lexical hint.
fn region_bounds(path:&Path,lines:&[&str],declaration:usize)->Option<(usize,usize)> {
    let extension=path.extension()?.to_str()?;
    if extension=="py" {
        if !lines[declaration].trim_end().ends_with(':') {return None;}
        let baseline=lines[declaration].len()-lines[declaration].trim_start().len();
        let mut end=declaration;
        for (index,line) in lines.iter().enumerate().skip(declaration+1) {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {continue;}
            let indent=line.len()-line.trim_start().len();
            if indent<=baseline {break;}
            end=index;
        }
        return (end>declaration).then_some((declaration+1,end+1));
    }
    let mut depth=0i32;
    let mut opened=false;
    let mut block_comment=false;
    let mut quote:Option<char>=None;
    let mut escaped=false;
    // Bound work per match; large/malformed source must not stall grep.
    for (idx,line) in lines.iter().enumerate().skip(declaration).take(2000) {
        let chars:Vec<char>=line.chars().collect();
        let mut pos=0;
        while pos<chars.len() {
            let ch=chars[pos];
            let next=chars.get(pos+1).copied();
            if block_comment {
                if ch=='*' && next==Some('/') {block_comment=false;pos+=2;} else {pos+=1;}
                continue;
            }
            if let Some(delimiter)=quote {
                if escaped {escaped=false;}
                else if ch=='\\' {escaped=true;}
                else if ch==delimiter {quote=None;}
                pos+=1;
                continue;
            }
            if ch=='/' && next==Some('/') {break;}
            if ch=='/' && next==Some('*') {block_comment=true;pos+=2;continue;}
            if matches!(ch,'"'|'\''|'\x60') {quote=Some(ch);pos+=1;continue;}
            if ch=='{' {depth+=1;opened=true;}
            if ch=='}' && opened {
                depth-=1;
                if depth==0{return Some((declaration+1,idx+1));}
                if depth<0{return None;}
            }
            if !opened && ch==';' {return None;}
            pos+=1;
        }
        if !opened && idx>declaration+8 {return None;}
        // Unterminated single-quoted strings are not a reliable parse.
        if quote==Some('\'') {return None;}
    }
    None
}

/// For a matching one-based line, return at most one nearby named declaration.
/// `distance` is an honest line displacement, NOT a semantic containment claim.
/// Unknown languages, binary data and matches without nearby declarations get
/// no invented symbols. Call only for real regex matches from bounded grep.
pub fn nearest_declaration(path: &Path, source: &str, line_number: usize) -> Option<Value> {
    if !has_supported_extension(path) || line_number == 0 {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    if lines.len() < line_number || lines.len()>100_000 { return None; }
    let first = line_number.saturating_sub(MAX_SCAN_BACK_LINES + 1);
    for index in (first..line_number).rev() {
        let line = lines[index];
        if line.len() > 4096 { continue; }
        if let Some(captures) = declaration_pattern().captures(line) {
            let kind = captures.get(1)?.as_str();
            let name = captures.get(2)?.as_str();
            let region=region_bounds(path,&lines,index);
            if region.is_some_and(|(_,end)|line_number>end) {continue;}
            let mut result=json!({
                "kind": kind,
                "name": name,
                "declaration_line": index + 1,
                "distance": line_number - (index + 1),
                "declaration": line.trim().chars().take(MAX_DECLARATION_CHARS).collect::<String>(),
                "confidence": "lexical_nearest_preceding"
            });
            if let Some((start,end))=region {
                result["region"]=json!({"start_line":start,"end_line":end,"method":
                    if path.extension().and_then(|e|e.to_str())==Some("py") {
                        "python_indentation"
                    } else {"balanced_braces"}
                });
            }
            return Some(result);
        }
        if matches!(path.extension().and_then(|e| e.to_str()), Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")) {
            if let Some(captures) = arrow_pattern().captures(line) {
                let region=region_bounds(path,&lines,index);
                if region.is_some_and(|(_,end)|line_number>end) {continue;}
                let mut result=json!({
                    "kind": "arrow_function",
                    "name": captures.get(1)?.as_str(),
                    "declaration_line": index + 1,
                    "distance": line_number - (index + 1),
                    "declaration": line.trim().chars().take(MAX_DECLARATION_CHARS).collect::<String>(),
                    "confidence": "lexical_nearest_preceding"
                });
                if let Some((start,end))=region {
                    result["region"]=json!({"start_line":start,"end_line":end,"method":"balanced_braces"});
                }
                return Some(result);
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
    fn bounded_regions_respect_python_dedent_and_rust_braces() {
        let rust="fn earlier() {\n let here = 1;\n}\nfn later() {\n let inside = 2;\n}\n";
        let in_later=nearest_declaration(Path::new("lib.rs"),rust,5).unwrap();
        assert_eq!(in_later["name"],"later");
        assert_eq!(in_later["region"]["start_line"],4);
        assert_eq!(in_later["region"]["end_line"],6);
        let python="def one():\n  return 1\ndef two():\n  value = 2\n  return value\n";
        let in_two=nearest_declaration(Path::new("main.py"),python,5).unwrap();
        assert_eq!(in_two["name"],"two");
        assert_eq!(in_two["region"]["method"],"python_indentation");
        assert_eq!(in_two["region"]["end_line"],5);
    }

    #[test]
    fn no_unbounded_scan_or_fake_symbol_context() {
        let many = format!("fn ancient() {{}}\n{}hit\n", "\n".repeat(300));
        assert!(nearest_declaration(Path::new("lib.rs"), &many, 302).is_none());
        assert!(nearest_declaration(Path::new("lib.rs"), "let x = 2;\n", 1).is_none());
        assert!(nearest_declaration(Path::new("lib.rs"), "", 1).is_none());
    }
}
