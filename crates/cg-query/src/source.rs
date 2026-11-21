//! A bounded, current-on-disk source view. Source files can be modified in place by an editor,
//! so unlike immutable index segments they are read into owned bytes rather than mmap'd.

use std::io::Read;
use std::ops::Index;
use std::path::Path;
use std::sync::OnceLock;

pub struct SourceView {
    text: String,
    starts: OnceLock<Vec<usize>>,
}

impl SourceView {
    pub fn read(path: &Path, max_bytes: usize) -> std::io::Result<Self> {
        cg_core::cancel::check_now();
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.take((max_bytes as u64).saturating_add(1)).read_to_end(&mut bytes)?;
        cg_core::cancel::check_now();
        if bytes.len() > max_bytes {
            return Err(std::io::Error::other("source exceeds the per-file read budget"));
        }
        let text = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        Ok(Self { text, starts: OnceLock::new() })
    }
    fn starts(&self) -> &[usize] {
        self.starts.get_or_init(|| {
            let mut starts = vec![0];
            for i in memchr::memchr_iter(b'\n', self.text.as_bytes()) {
                if i + 1 < self.text.len() {
                    starts.push(i + 1);
                }
            }
            if self.text.is_empty() {
                starts.clear();
            }
            starts
        })
    }
    pub fn len(&self) -> usize {
        self.starts().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 1-based line accessor.
    pub fn line(&self, n: u32) -> Option<&str> {
        let idx = n.checked_sub(1)? as usize;
        (idx < self.len()).then(|| &self[idx])
    }
}

impl Index<usize> for SourceView {
    type Output = str;
    fn index(&self, i: usize) -> &str {
        let starts = self.starts();
        let end = starts.get(i + 1).copied().unwrap_or(self.text.len());
        let line = &self.text[starts[i]..end];
        match line.strip_suffix('\n') {
            Some(line) => line.strip_suffix('\r').unwrap_or(line),
            None => line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_current_content_without_per_line_strings_and_bounds_growth() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "α\r\n\nlast\n").unwrap();
        let s = SourceView::read(file.path(), 32).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(&s[0], "α");
        assert_eq!(&s[1], "");
        assert_eq!(&s[2], "last");
        std::fs::write(file.path(), "new\n").unwrap();
        assert_eq!(&SourceView::read(file.path(), 32).unwrap()[0], "new");
        assert!(SourceView::read(file.path(), 2).is_err());
        std::fs::write(file.path(), "").unwrap();
        assert!(SourceView::read(file.path(), 32).unwrap().is_empty());
        std::fs::write(file.path(), "last\r").unwrap();
        assert_eq!(&SourceView::read(file.path(), 32).unwrap()[0], "last\r");
    }
}
