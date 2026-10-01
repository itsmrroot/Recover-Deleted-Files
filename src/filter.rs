//! User-supplied selection criteria shared by `scan` and `recover`.

use std::collections::HashSet;

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};

use crate::carve::{self, Carved, Category, Format};
use crate::fs::DeletedFile;

#[derive(Default)]
pub struct Filter {
    /// Normalized extensions (`jpg`, `mp4`, ...).
    pub exts: Option<HashSet<String>>,
    pub categories: Option<HashSet<Category>>,
    /// Glob on the file name, or on the full path if it contains `/`.
    pub name: Option<(GlobMatcher, bool)>,
    pub min_size: u64,
    pub max_size: Option<u64>,
}

impl Filter {
    pub fn new(
        exts: &[String],
        categories: &[Category],
        name: Option<&str>,
        min_size: u64,
        max_size: Option<u64>,
    ) -> Result<Self> {
        let name = name
            .map(|g| -> Result<_> {
                let m = GlobBuilder::new(g)
                    .case_insensitive(true)
                    .literal_separator(false)
                    .build()
                    .with_context(|| format!("invalid --name pattern {g:?}"))?
                    .compile_matcher();
                Ok((m, g.contains('/')))
            })
            .transpose()?;
        Ok(Self {
            exts: (!exts.is_empty()).then(|| exts.iter().map(|e| carve::normalize_ext(e)).collect()),
            categories: (!categories.is_empty()).then(|| categories.iter().copied().collect()),
            name,
            min_size,
            max_size,
        })
    }

    fn size_ok(&self, size: u64) -> bool {
        size >= self.min_size && self.max_size.is_none_or(|m| size <= m)
    }

    fn type_ok(&self, ext: &str, category: Option<Category>) -> bool {
        let ext = carve::normalize_ext(ext);
        self.exts.as_ref().is_none_or(|s| s.contains(&ext))
            && self.categories.as_ref().is_none_or(|s| category.is_some_and(|c| s.contains(&c)))
    }

    pub fn matches_file(&self, f: &DeletedFile) -> bool {
        let ext = f.name().rsplit_once('.').map_or("", |(_, e)| e);
        self.size_ok(f.size)
            && self.type_ok(ext, carve::category_for_ext(ext))
            && self.name.as_ref().is_none_or(|(m, full)| m.is_match(if *full { f.path.as_str() } else { f.name() }))
    }

    pub fn matches_carved(&self, c: &Carved) -> bool {
        self.size_ok(c.len) && self.type_ok(c.ext, Some(c.category))
    }

    /// Whether any output of `f` could pass the type filters.
    pub fn wants_format(&self, f: &dyn Format) -> bool {
        f.kinds().iter().any(|(ext, cat)| self.type_ok(ext, Some(*cat)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Condition, FileData};

    fn file(path: &str, size: u64) -> DeletedFile {
        DeletedFile {
            id: 1,
            path: path.into(),
            size,
            created: None,
            modified: None,
            condition: Condition::Recoverable,
            note: None,
            data: FileData::Lost,
        }
    }

    #[test]
    fn filters_by_type_name_and_size() {
        let f = Filter::new(&[], &[Category::Image], Some("IMG_*"), 10, None).unwrap();
        assert!(f.matches_file(&file("DCIM/100/img_0001.JPEG", 100)));
        assert!(!f.matches_file(&file("DCIM/100/img_0001.mov", 100)));
        assert!(!f.matches_file(&file("DCIM/100/img_0001.jpg", 5)));
        assert!(!f.matches_file(&file("DCIM/100/dsc_0001.jpg", 100)));

        let f = Filter::new(&["jpeg".into()], &[], Some("Users/*/Pictures/**"), 0, None).unwrap();
        assert!(f.matches_file(&file("Users/bob/Pictures/2024/a.jpg", 1)));
        assert!(!f.matches_file(&file("Users/bob/Desktop/a.jpg", 1)));
    }
}
