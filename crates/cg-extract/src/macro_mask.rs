//! Rust conditional item wrappers (`cfg_rt! { pub struct X; impl X { .. } }`, Tokio's
//! `cfg_*!` family) hide whole items from tree-sitter: the body is an opaque token tree, so
//! the kernel extracts neither the declarations nor the calls inside them.
//!
//! [`rust_item_wrappers`] returns a *position-preserving* view of the source in which only
//! the wrapper's `name! {` and its closing `}` are blanked (newlines kept, every other byte
//! unchanged), so line/column positions, and therefore node ids, of everything else stay
//! identical. Arbitrary macros, `cfg_if!`, macro definitions and wrappers whose body does not
//! close are left opaque; nothing is expanded or executed. The caller compares the number of
//! syntax errors before and after blanking and keeps the original text when blanking added any.

use std::time::{Duration, Instant};
use tree_sitter::{Node, ParseOptions, Parser, Tree};

/// Largest source this pre-pass touches.
pub const MAX_BYTES: usize = 1024 * 1024;

/// Outcome of the wrapper pre-pass.
pub enum Mask {
    /// No conditional item wrappers (or the file is too large): extract the original text.
    None,
    /// The pre-pass parse ran out of time: the caller flags the file as timed out.
    Expired,
    /// The masked text and the number of ERROR / MISSING nodes in the unmasked parse.
    Masked(String, usize),
}

fn wrapper(name: &str) -> bool {
    let leaf = name.rsplit("::").next().unwrap_or(name);
    leaf == "pin_project" || (leaf.starts_with("cfg_") && leaf != "cfg_if" && leaf.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
}

fn collect(n: Node, src: &str, ranges: &mut Vec<(usize, usize)>, depth: usize) {
    if depth > 128 || n.kind() == "macro_definition" {
        return;
    }
    let mut c = n.walk();
    let children: Vec<Node> = n.children(&mut c).collect();
    if matches!(n.kind(), "macro_invocation" | "token_tree") {
        let mut i = 0;
        while i < children.len() {
            if i + 2 < children.len() && children[i + 1].kind() == "!" && children[i + 2].kind() == "token_tree" {
                let name = &src[children[i].byte_range()];
                let body = children[i + 2];
                // Only a wrapper whose body really closes: tree-sitter recovers an unclosed
                // `cfg_x! {` with a zero-width MISSING `}`, and blanking `end - 1` would then
                // hit a real byte of the last token.
                let closed = body
                    .child(body.child_count().saturating_sub(1))
                    .is_some_and(|l| l.kind() == "}" && !l.is_missing() && l.start_byte() + 1 == body.end_byte());
                if wrapper(name) && src.as_bytes().get(body.start_byte()) == Some(&b'{') && closed && body.end_byte() > body.start_byte() + 1 {
                    ranges.push((children[i].start_byte(), body.start_byte() + 1));
                    ranges.push((body.end_byte() - 1, body.end_byte()));
                    collect(body, src, ranges, depth + 1);
                }
                // An unknown macro's tokens are not ordinary Rust items.
                i += 3;
            } else {
                collect(children[i], src, ranges, depth + 1);
                i += 1;
            }
        }
    } else {
        for ch in children {
            collect(ch, src, ranges, depth + 1);
        }
    }
}

/// Byte ranges to blank for the wrappers found in `tree` (empty when there are none).
pub fn wrapper_ranges(tree: &Tree, src: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if src.contains("cfg_") || src.contains("pin_project") {
        collect(tree.root_node(), src, &mut ranges, 0);
    }
    ranges
}

/// `src` with the given ranges blanked (newlines kept).
pub fn blank_ranges(src: &str, ranges: &[(usize, usize)]) -> String {
    let mut masked = src.as_bytes().to_vec();
    for &(start, end) in ranges {
        for b in &mut masked[start..end] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    }
    // Ranges start and end on node boundaries, so whole characters are replaced by ASCII
    // spaces and the result is still valid UTF-8.
    String::from_utf8(masked).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Number of ERROR and MISSING nodes in a tree (descends into error-bearing subtrees only).
pub fn error_nodes(tree: &Tree) -> usize {
    let mut count = 0usize;
    let mut cursor = tree.root_node().walk();
    let mut visited = 0usize;
    'outer: loop {
        let n = cursor.node();
        if n.is_error() || n.is_missing() {
            count += 1;
        }
        visited += 1;
        if visited > 2_000_000 {
            break;
        }
        if n.has_error() && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                break 'outer;
            }
        }
    }
    count
}

/// Fast linear scan: does `src` contain a `cfg_<ident>! {` (excluding `cfg_if!`) or `pin_project! {` candidate?
/// Avoids parsing files that only mention `cfg_attr`, `cfg_if!`, or unrelated identifiers.
fn has_wrapper_candidate(src: &str) -> bool {
    let bytes = src.as_bytes();
    let follows_bang_brace = |mut j: usize| -> bool {
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'!' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            return j < bytes.len() && bytes[j] == b'{';
        }
        false
    };
    let mut pos = 0;
    while let Some(off) = memchr::memmem::find(&bytes[pos..], b"cfg_") {
        let start = pos + off;
        let prev_ident = start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let mut i = start + 4;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if !prev_ident && &bytes[start..i] != b"cfg_if" && follows_bang_brace(i) {
            return true;
        }
        pos = i;
    }
    pos = 0;
    while let Some(off) = memchr::memmem::find(&bytes[pos..], b"pin_project") {
        let start = pos + off;
        let prev_ident = start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let end = start + b"pin_project".len();
        if !prev_ident && follows_bang_brace(end) {
            return true;
        }
        pos = end;
    }
    false
}

thread_local! {
    static RUST_PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

/// The masked view of a Rust source (see the module docs).
pub fn rust_item_wrappers(src: &str, timeout: Duration) -> Mask {
    if src.len() > MAX_BYTES || !has_wrapper_candidate(src) {
        return Mask::None;
    }
    if timeout.is_zero() {
        return Mask::Expired;
    }
    let bytes = src.as_bytes();
    let deadline = Instant::now() + timeout;
    let mut expired = false;
    let mut cancel = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            expired = true;
            true
        } else {
            false
        }
    };
    let tree = RUST_PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let language = cg_kernel::hooks::language("rust")?;
            let mut p = Parser::new();
            p.set_language(&language).ok()?;
            *slot = Some(p);
        }
        let parser = slot.as_mut().unwrap();
        parser.reset();
        parser.parse_with_options(&mut |offset, _| bytes.get(offset..).unwrap_or_default(), None, Some(ParseOptions::new().progress_callback(&mut cancel)))
    });
    if expired {
        return Mask::Expired;
    }
    let Some(tree) = tree else { return Mask::None };
    let ranges = wrapper_ranges(&tree, src);
    if ranges.is_empty() {
        return Mask::None;
    }
    Mask::Masked(blank_ranges(src, &ranges), error_nodes(&tree))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(src: &str) -> (String, usize) {
        match rust_item_wrappers(src, Duration::from_secs(5)) {
            Mask::Masked(m, e) => (m, e),
            Mask::None => panic!("expected a mask"),
            Mask::Expired => panic!("unexpected timeout"),
        }
    }

    #[test]
    fn blanks_only_wrapper_tokens() {
        let src = "cfg_rt! {\n    pub struct A;\n    impl A { fn f(&self) {} }\n}\ncfg_if::cfg_if! { if #[cfg(x)] { fn g() {} } }\nfoo! { fn h() {} }\n";
        let (masked, errors) = masked(src);
        assert_eq!(errors, 0);
        assert_eq!(masked.len(), src.len());
        assert_eq!(masked.lines().count(), src.lines().count());
        assert!(masked.starts_with("         \n    pub struct A;"));
        assert!(masked.contains("cfg_if::cfg_if!"));
        assert!(masked.contains("foo! { fn h() {} }"));
        // positions of untouched text are unchanged
        assert_eq!(masked.find("impl A"), src.find("impl A"));
    }

    #[test]
    fn nested_wrappers_and_impl_bodies() {
        let src = "impl S {\n    cfg_net! {\n        cfg_unix! {\n            fn a(&self) {}\n        }\n    }\n}\n";
        let (masked, _) = masked(src);
        assert!(!masked.contains("cfg_"));
        assert_eq!(masked.find("fn a"), src.find("fn a"));
    }

    #[test]
    fn untouched_without_wrappers() {
        assert!(matches!(rust_item_wrappers("fn main() { cfg_x(); }", Duration::from_secs(5)), Mask::None));
        assert!(matches!(rust_item_wrappers("cfg_rt! { fn a() {} }", Duration::ZERO), Mask::Expired));
        assert!(matches!(rust_item_wrappers("macro_rules! cfg_rt { ($($i:item)*) => { $($i)* } }", Duration::from_secs(5)), Mask::None));
    }

    #[test]
    fn unclosed_wrapper_is_left_alone() {
        let src = "cfg_rt! {\n    fn a() {}\n    let s = \"\u{e9}\";\n";
        match rust_item_wrappers(src, Duration::from_secs(5)) {
            Mask::None => {}
            Mask::Masked(m, _) => assert_eq!(m, src, "an unclosed wrapper must not be blanked"),
            Mask::Expired => panic!("unexpected timeout"),
        }
    }

    #[test]
    fn counts_error_nodes() {
        let mut p = Parser::new();
        p.set_language(&cg_kernel::hooks::language("rust").unwrap()).unwrap();
        assert_eq!(error_nodes(&p.parse("fn a() {}", None).unwrap()), 0);
        assert!(error_nodes(&p.parse("fn a( {", None).unwrap()) >= 1);
        assert_eq!(masked("cfg_rt! { fn a() {} }\n").1, 0);
        assert!(masked("cfg_rt! { fn a( {} }\nfn b() {}\n").1 >= 1);
    }
}
