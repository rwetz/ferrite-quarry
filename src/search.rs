//! Original trigram implementation, inspired by logscope's candidate/verify design.
//! No logscope source or private dependencies are copied into Quarry.
use regex::RegexBuilder;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

pub const MAX_FILE: u64 = 2 * 1024 * 1024;
pub const MAX_TEXT: usize = 32 * 1024 * 1024;
pub const MAX_DOCS: usize = 200_000;
pub const MAX_POSTINGS: usize = 4_000_000;
pub const MAX_RESULTS: usize = 500;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Scope {
    #[default]
    All,
    Names,
    Content,
}

#[derive(Clone)]
pub struct Document {
    pub path: Arc<PathBuf>,
    pub line: Option<usize>,
    pub text: String,
    normalized: String,
}

#[derive(Default)]
pub struct Catalog {
    pub docs: Vec<Document>,
    postings: HashMap<[u8; 3], Vec<u32>>,
    pub files: usize,
    pub skipped: usize,
    pub errors: usize,
    pub capped: bool,
    pub warnings: Vec<String>,
    text_bytes: usize,
    posting_count: usize,
}

#[derive(Clone)]
pub struct Hit {
    pub path: Arc<PathBuf>,
    pub line: Option<usize>,
    pub text: String,
}
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
}

impl Catalog {
    fn insert(&mut self, path: Arc<PathBuf>, line: Option<usize>, text: String) -> bool {
        if self.docs.len() >= MAX_DOCS || self.text_bytes.saturating_add(text.len()) > MAX_TEXT {
            self.capped = true;
            return false;
        }
        let normalized = text.to_lowercase();
        let id = self.docs.len() as u32;
        let mut grams: Vec<[u8; 3]> = normalized
            .as_bytes()
            .windows(3)
            .map(|g| [g[0], g[1], g[2]])
            .collect();
        grams.sort_unstable();
        grams.dedup();
        if self.posting_count.saturating_add(grams.len()) > MAX_POSTINGS {
            self.capped = true;
            return false;
        }
        self.posting_count += grams.len();
        for gram in grams {
            self.postings.entry(gram).or_default().push(id);
        }
        self.text_bytes += text.len();
        self.docs.push(Document {
            path,
            line,
            text,
            normalized,
        });
        true
    }

    fn candidates(&self, needle: &str, cancel: &AtomicBool) -> Vec<u32> {
        if needle.len() < 3 {
            return (0..self.docs.len() as u32).collect();
        }
        let grams: HashSet<[u8; 3]> = needle
            .as_bytes()
            .windows(3)
            .map(|g| [g[0], g[1], g[2]])
            .collect();
        let mut lists = Vec::new();
        for gram in grams {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let Some(list) = self.postings.get(&gram) else {
                return Vec::new();
            };
            lists.push(list);
        }
        lists.sort_unstable_by_key(|list| list.len());
        lists[0]
            .iter()
            .copied()
            .take_while(|_| !cancel.load(Ordering::Relaxed))
            .filter(|id| lists[1..].iter().all(|list| list.binary_search(id).is_ok()))
            .collect()
    }

    pub fn search(
        &self,
        query: &str,
        scope: Scope,
        regex: bool,
        cancel: &AtomicBool,
    ) -> Result<Results, String> {
        if query.trim().is_empty() {
            return Ok(Results {
                hits: Vec::new(),
                total: 0,
            });
        }
        let needle = query.to_lowercase();
        let pattern = regex
            .then(|| {
                RegexBuilder::new(query)
                    .case_insensitive(true)
                    .size_limit(2 * 1024 * 1024)
                    .build()
            })
            .transpose()
            .map_err(|e| format!("Invalid regular expression: {e}"))?;
        let ids = if regex {
            (0..self.docs.len() as u32).collect()
        } else {
            self.candidates(&needle, cancel)
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("Search cancelled".into());
        }
        let mut hits = Vec::new();
        let mut total = 0;
        for id in ids {
            if cancel.load(Ordering::Relaxed) {
                return Err("Search cancelled".into());
            }
            let doc = &self.docs[id as usize];
            if (scope == Scope::Names && doc.line.is_some())
                || (scope == Scope::Content && doc.line.is_none())
            {
                continue;
            }
            let matched = pattern.as_ref().map_or_else(
                || doc.normalized.contains(&needle),
                |pattern| pattern.is_match(&doc.text),
            );
            if matched {
                total += 1;
                if hits.len() < MAX_RESULTS {
                    hits.push(Hit {
                        path: doc.path.clone(),
                        line: doc.line,
                        text: snippet(&doc.text, 220),
                    });
                }
            }
        }
        Ok(Results { hits, total })
    }
}

pub fn snippet(text: &str, max: usize) -> String {
    let text = text.trim();
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push_str("...");
    }
    result
}

pub fn scan(roots: &[PathBuf], cancel: &AtomicBool, progress: &AtomicUsize) -> Option<Catalog> {
    let mut catalog = Catalog::default();
    let mut seen = HashSet::new();
    for root in roots {
        let root = match root.canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ => {
                catalog.errors += 1;
                catalog
                    .warnings
                    .push(format!("Unavailable folder: {}", root.display()));
                continue;
            }
        };
        let walker = ignore::WalkBuilder::new(&root)
            .follow_links(false)
            .require_git(false)
            .filter_entry(|entry| {
                !entry.file_type().is_some_and(|kind| kind.is_dir())
                    || !matches!(
                        entry.file_name().to_str(),
                        Some(
                            ".git" | "target" | "node_modules" | ".next" | "__pycache__" | ".venv"
                        )
                    )
            })
            .build();
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    catalog.errors += 1;
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            // Avoid duplicate documents when configured roots overlap.
            let path = entry.path().to_path_buf();
            if !seen.insert(path.clone()) {
                continue;
            }
            let path = Arc::new(path);
            if !catalog.insert(path.clone(), None, path.to_string_lossy().into_owned()) {
                break;
            }
            catalog.files += 1;
            progress.store(catalog.files, Ordering::Relaxed);
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    catalog.errors += 1;
                    continue;
                }
            };
            if metadata.len() > MAX_FILE {
                catalog.skipped += 1;
                continue;
            }
            let file = match std::fs::File::open(path.as_ref()) {
                Ok(file) => file,
                Err(_) => {
                    catalog.errors += 1;
                    continue;
                }
            };
            let mut bytes = Vec::new();
            if file.take(MAX_FILE + 1).read_to_end(&mut bytes).is_err() {
                catalog.errors += 1;
                continue;
            }
            if bytes.len() as u64 > MAX_FILE || bytes.contains(&0) {
                catalog.skipped += 1;
                continue;
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    catalog.skipped += 1;
                    continue;
                }
            };
            for (line, text) in text.lines().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                if text.len() > 16 * 1024 {
                    catalog.skipped += 1;
                    continue;
                }
                if !catalog.insert(path.clone(), Some(line + 1), text.to_string()) {
                    break;
                }
            }
            if catalog.capped {
                break;
            }
        }
        if catalog.capped {
            break;
        }
    }
    Some(catalog)
}

pub fn preview(path: &Path, line: Option<usize>) -> Result<Vec<(usize, String)>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("This file is no longer a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE || bytes.contains(&0) {
        return Err("Preview unavailable for binary files or files larger than 2 MiB".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "Preview requires UTF-8 text".to_string())?;
    let center = line.unwrap_or(1).saturating_sub(1);
    let start = center.saturating_sub(12);
    Ok(text
        .lines()
        .enumerate()
        .skip(start)
        .take(80)
        .map(|(i, text)| (i + 1, snippet(text, 500)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_literals_equal_brute_force_including_unicode_and_short_queries() {
        let mut catalog = Catalog::default();
        for text in [
            "hello world",
            "HELLO there",
            "éclair",
            "ababa",
            "a🙂b",
            "nothing",
            "résumé",
            "",
        ] {
            catalog.insert(Arc::new("test.txt".into()), Some(1), text.into());
        }
        for needle in ["hello", "é", "aba", "a", "🙂", "rés", "absent", "wor"] {
            let result = catalog
                .search(needle, Scope::All, false, &AtomicBool::new(false))
                .unwrap();
            let expected = catalog
                .docs
                .iter()
                .filter(|doc| doc.normalized.contains(&needle.to_lowercase()))
                .count();
            assert_eq!(result.total, expected, "{needle}");
        }
    }
    #[test]
    fn filesystem_scan_respects_ignore_files_binary_files_and_overlapping_roots() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(temp.path().join("ignored.txt"), "secret needle").unwrap();
        std::fs::write(temp.path().join("found.txt"), "first\nneedle here\nlast").unwrap();
        std::fs::write(temp.path().join("binary.bin"), b"needle\0").unwrap();
        let cancel = AtomicBool::new(false);
        let catalog = scan(
            &[temp.path().into(), temp.path().into()],
            &cancel,
            &AtomicUsize::new(0),
        )
        .unwrap();
        assert_eq!(catalog.files, 2);
        let results = catalog
            .search("needle", Scope::Content, false, &cancel)
            .unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.hits[0].line, Some(2));
        assert_eq!(
            preview(&results.hits[0].path, Some(2)).unwrap()[1],
            (2, "needle here".into())
        );
        assert!(catalog.search("[", Scope::All, true, &cancel).is_err());
        assert_eq!(
            catalog
                .search("ne+dle", Scope::Content, true, &cancel)
                .unwrap()
                .total,
            1
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(scan(&[temp.path().into()], &cancel, &AtomicUsize::new(0)).is_none());
    }

    #[test]
    fn candidate_verification_rejects_reordered_and_disconnected_trigrams() {
        let mut catalog = Catalog::default();
        for text in ["abc x bcd", "abcd", "bcda", "ab"] {
            catalog.insert(Arc::new("test.txt".into()), Some(1), text.into());
        }
        let result = catalog
            .search("abcd", Scope::Content, false, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].text, "abcd");
    }

    #[test]
    fn generated_substrings_match_brute_force() {
        let mut catalog = Catalog::default();
        for i in 0..120 {
            catalog.insert(
                Arc::new("test.txt".into()),
                Some(i + 1),
                format!("file_{i:04} cafe_{} / αβ🙂 / item{}", i % 9, i % 17),
            );
        }
        for query in [
            "file", "000", "cafe_3", "item12", "αβ", "🙂", " / ", "FILE_004", "_0", "nope",
        ] {
            let result = catalog
                .search(query, Scope::Content, false, &AtomicBool::new(false))
                .unwrap();
            let expected: Vec<_> = catalog
                .docs
                .iter()
                .filter(|doc| doc.normalized.contains(&query.to_lowercase()))
                .map(|doc| doc.line)
                .collect();
            assert_eq!(
                result.hits.iter().map(|hit| hit.line).collect::<Vec<_>>(),
                expected,
                "{query}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_do_not_expand_the_search_scope() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private.txt"), "outside-only").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked-folder")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("private.txt"),
            root.path().join("linked-file"),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let catalog = scan(&[root.path().into()], &cancel, &AtomicUsize::new(0)).unwrap();
        assert_eq!(catalog.files, 0);
        assert_eq!(
            catalog
                .search("outside-only", Scope::Content, false, &cancel)
                .unwrap()
                .total,
            0
        );
    }
}
