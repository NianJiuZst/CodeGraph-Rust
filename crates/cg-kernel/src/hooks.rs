//! Patch P4 - parse hook with thread-local cached parsers and parse timeouts.
//!
//! Every walker parses through [`parse_lang`] (or [`parse_lang_with_rewrite`]), which:
//!  1. reuses one `tree_sitter::Parser` per language per worker thread instead of
//!     allocating and calling `set_language` on every file,
//!  2. enforces the per-file parse timeout (design doc 4.4) inside tree-sitter's
//!     already-sampled progress callback (~every 100 parser ops), and
//!  3. stashes a clone of the finished syntax tree in a thread-local so that
//!     `cg-extract` can build facts tables from the SAME parse instead of parsing twice.
//!
//! A `Tree` clone is a reference-count bump, so the stash is free.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use tree_sitter::{ParseOptions, Parser, Tree};

const LANG_SLOTS: usize = 14;

fn lang_slot(lang: &str) -> Option<usize> {
    match lang {
        "typescript" => Some(0),
        "tsx" => Some(1),
        "javascript" | "jsx" => Some(2),
        "java" => Some(3),
        "python" => Some(4),
        "go" => Some(5),
        "rust" => Some(6),
        "kotlin" => Some(7),
        "c" => Some(8),
        "cpp" => Some(9),
        "csharp" => Some(10),
        "ruby" => Some(11),
        "php" => Some(12),
        "swift" => Some(13),
        _ => None,
    }
}

thread_local! {
    static STASH: RefCell<Option<Tree>> = const { RefCell::new(None) };
    static TIMEOUT: Cell<Option<Duration>> = const { Cell::new(None) };
    static TIMED_OUT: Cell<bool> = const { Cell::new(false) };
    static PARSERS: RefCell<[Option<Parser>; LANG_SLOTS]> =
        const { RefCell::new([const { None }; LANG_SLOTS]) };
}

/// Per-thread parse deadline for the next extractions (`None` = unlimited).
pub fn set_parse_timeout(limit: Option<Duration>) {
    TIMEOUT.with(|c| c.set(limit));
}

/// The syntax tree of the most recent successful parse on this thread.
pub fn take_tree() -> Option<Tree> {
    STASH.with(|s| s.borrow_mut().take())
}

/// Whether the most recent parse on this thread was cancelled by the timeout.
pub fn parse_timed_out() -> bool {
    TIMED_OUT.with(|c| c.get())
}

/// Drop any stashed tree (call after a failed extraction).
pub fn clear() {
    STASH.with(|s| *s.borrow_mut() = None);
}

fn with_parser<T>(lang: &str, f: impl FnOnce(&mut Parser) -> Result<T, String>) -> Result<T, String> {
    let slot = lang_slot(lang).ok_or_else(|| format!("no {lang} grammar"))?;
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        if parsers[slot].is_none() {
            let grammar = crate::langs::grammar_for(lang)
                .ok_or_else(|| format!("no {lang} grammar"))?;
            let mut parser = Parser::new();
            parser
                .set_language(&grammar)
                .map_err(|e| format!("set_language({lang}) failed: {e}"))?;
            parsers[slot] = Some(parser);
        }
        f(parsers[slot].as_mut().unwrap())
    })
}

fn parse_with_parser(parser: &mut Parser, source: &str) -> Result<Tree, String> {
    STASH.with(|s| *s.borrow_mut() = None);
    TIMED_OUT.with(|c| c.set(false));
    let bytes = source.as_bytes();
    let tree = match TIMEOUT.with(|c| c.get()) {
        None => parser.parse(bytes, None),
        Some(limit) => {
            if limit.is_zero() {
                TIMED_OUT.with(|c| c.set(true));
                return Err("timeout: parse exceeded budget".to_string());
            }
            let deadline = Instant::now() + limit;
            let mut expired = false;
            let mut progress = |_: &tree_sitter::ParseState| {
                if Instant::now() >= deadline {
                    expired = true;
                    true // cancel
                } else {
                    false
                }
            };
            let tree = parser.parse_with_options(
                &mut |offset, _| if offset < bytes.len() { &bytes[offset..] } else { &[][..] },
                None,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            );
            if tree.is_none() {
                parser.reset();
            }
            if expired {
                TIMED_OUT.with(|c| c.set(true));
                return Err("timeout: parse exceeded budget".to_string());
            }
            tree
        }
    };
    let Some(tree) = tree else {
        parser.reset();
        return Err("parser returned null tree".to_string());
    };
    STASH.with(|s| *s.borrow_mut() = Some(tree.clone()));
    Ok(tree)
}

/// Parse `source` using the thread-local cached parser for `lang`.
pub(crate) fn parse_lang(lang: &str, source: &str) -> Result<Tree, String> {
    with_parser(lang, |parser| parse_with_parser(parser, source))
}

/// Retry a completed error tree with an equal-byte-length lexical rewrite. The walker and facts
/// still read the original source. Accept the retry only when it removes every parse error and
/// preserve the original result if the bounded optional retry fails.
pub(crate) fn parse_lang_with_rewrite(
    lang: &str,
    source: &str,
    rewrite: impl FnOnce(&Tree, &str) -> Option<String>,
) -> Result<Tree, String> {
    with_parser(lang, |parser| {
        let start = Instant::now();
        let original = parse_with_parser(parser, source)?;
        if !original.root_node().has_error() {
            return Ok(original);
        }
        let Some(masked) = rewrite(&original, source) else {
            return Ok(original);
        };
        if masked.len() != source.len() {
            return Ok(original);
        }
        let limit = TIMEOUT.with(|c| c.get());
        let remaining = match limit {
            Some(limit) => match limit.checked_sub(start.elapsed()) {
                Some(left) if !left.is_zero() => Some(left),
                _ => return Ok(original),
            },
            None => None,
        };
        TIMEOUT.with(|c| c.set(remaining));
        let retry = parse_with_parser(parser, &masked);
        TIMEOUT.with(|c| c.set(limit));
        if let Ok(tree) = retry {
            if !tree.root_node().has_error() {
                return Ok(tree);
            }
        }
        TIMED_OUT.with(|c| c.set(false));
        STASH.with(|s| *s.borrow_mut() = Some(original.clone()));
        Ok(original)
    })
}

/// The tree-sitter grammar the kernel uses for `lang` (tests and dev tools; the facts
/// extractor never needs it because it reuses the kernel's tree).
pub fn language(lang: &str) -> Option<tree_sitter::Language> {
    crate::langs::grammar_for(lang)
}
