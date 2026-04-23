//! On-demand method-signature extraction via tree-sitter. Called by `lookup`
//! after file_ids have been resolved to real paths, so we only re-parse the
//! files that actually contain matches.

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::{Node, Parser};

const METHOD_LIKE_KINDS: &[&str] = &[
    "method_declaration",
    "constructor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
];

/// Parse `path` and return `line_start (1-based) -> signature` for every
/// method-like declaration found. Returns an empty map on any error (missing
/// file, parse failure, tree-sitter setup failure) so callers can silently
/// fall back to name-only output.
pub fn extract_signatures(path: &Path) -> HashMap<u32, String> {
    let Ok(src) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_c_sharp::language())
        .is_err()
    {
        return HashMap::new();
    }
    let Some(tree) = parser.parse(&src, None) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    walk(tree.root_node(), src.as_bytes(), &mut out);
    out
}

fn walk(node: Node<'_>, src: &[u8], out: &mut HashMap<u32, String>) {
    if METHOD_LIKE_KINDS.contains(&node.kind()) {
        if let Some(sig) = signature_for(node, src) {
            let line = node.start_position().row as u32 + 1;
            out.insert(line, sig);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, out);
    }
}

/// Slice the source from the first non-attribute child of the method node up
/// to the end of the parameter list (or start of body, as a fallback), then
/// normalize whitespace. Drops `[Attribute]` lines and the method body.
fn signature_for(node: Node<'_>, src: &[u8]) -> Option<String> {
    // Start after any leading attribute_list children.
    let mut start = node.start_byte();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "attribute_list" {
            start = child.start_byte();
            break;
        }
    }

    let end = node
        .child_by_field_name("parameters")
        .map(|p| p.end_byte())
        .or_else(|| node.child_by_field_name("body").map(|b| b.start_byte()))
        .unwrap_or(node.end_byte());
    if end <= start {
        return None;
    }

    let raw = std::str::from_utf8(src.get(start..end)?).ok()?;
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let trimmed = out.trim().trim_end_matches(|c: char| c == '{').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_method_and_ctor_signatures() {
        let dir = std::env::temp_dir().join(format!("nspect-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = r#"
namespace N {
    public class C {
        public C(int x) { }
        [Obsolete]
        public async System.Threading.Tasks.Task<int> Foo(string name, bool strict) {
            return 0;
        }
        public int Bar() => 42;
    }
}
"#;
        let p = dir.join("C.cs");
        std::fs::write(&p, src).unwrap();
        let sigs = extract_signatures(&p);
        let on = |line: u32| sigs.get(&line).cloned().unwrap_or_default();
        // Line numbers match what compute_metrics records in metrics.yaml:
        // ctor on 4, Foo's span starts at its `[Obsolete]` attribute on 5,
        // Bar on 9. Signatures have attributes stripped.
        assert!(on(4).contains("C(int x)"), "{:?}", on(4));
        assert!(
            on(5).contains("Foo(string name, bool strict)"),
            "{:?}",
            on(5)
        );
        assert!(!on(5).contains("Obsolete"));
        assert!(on(9).contains("Bar()"), "{:?}", on(9));
        std::fs::remove_dir_all(&dir).ok();
    }
}
