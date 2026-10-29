use memchr::memmem::Finder;
use rustc_hash::FxHashSet;

use crate::db::{DbRecord, ItemKind};

#[derive(Debug, Clone)]
pub struct IndexItem {
    pub name: String,
    pub kind: ItemKind,
    pub category: String,
    pub path: String,
    /// Tampon contigu en minuscules `nom\0categorie\0chemin` pour recherche SIMD en une passe.
    search_haystack: String,
}

impl IndexItem {
    pub fn from_record(rec: DbRecord) -> Self {
        let mut search_haystack =
            String::with_capacity(rec.name.len() + rec.category.len() + rec.path.len() + 2);
        push_lowercase(&mut search_haystack, &rec.name);
        search_haystack.push('\0');
        push_lowercase(&mut search_haystack, &rec.category);
        search_haystack.push('\0');
        push_lowercase(&mut search_haystack, &rec.path);

        Self {
            name: rec.name,
            kind: rec.kind,
            category: rec.category,
            path: rec.path,
            search_haystack,
        }
    }

    pub fn update_category(&mut self, new_category: &str) {
        self.category.clear();
        self.category.push_str(new_category);

        self.search_haystack.clear();
        push_lowercase(&mut self.search_haystack, &self.name);
        self.search_haystack.push('\0');
        push_lowercase(&mut self.search_haystack, &self.category);
        self.search_haystack.push('\0');
        push_lowercase(&mut self.search_haystack, &self.path);
    }
}

#[inline]
fn push_lowercase(dst: &mut String, src: &str) {
    if src.is_ascii() {
        let start = dst.len();
        dst.push_str(src);
        unsafe {
            dst.as_bytes_mut()[start..].make_ascii_lowercase();
        }
    } else {
        for c in src.chars() {
            for lc in c.to_lowercase() {
                dst.push(lc);
            }
        }
    }
}

pub struct InMemoryIndex {
    items: Vec<IndexItem>,
    filtered_indices: Vec<u32>,
}

impl InMemoryIndex {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            filtered_indices: Vec::new(),
        }
    }

    pub fn replace_all(&mut self, records: Vec<DbRecord>, query: &str, category: &str) {
        self.items.clear();
        self.items.reserve(records.len());
        for rec in records {
            self.items.push(IndexItem::from_record(rec));
        }
        self.filter(query, category);
    }

    /// Filtre l'index en mémoire avec recherche sous-chaîne SIMD (`memchr::memmem`).
    pub fn filter(&mut self, query: &str, category: &str) {
        self.filtered_indices.clear();
        let query_trimmed = query.trim();

        let has_category = !category.is_empty();
        if query_trimmed.is_empty() {
            if !has_category {
                self.filtered_indices
                    .extend(0..self.items.len() as u32);
            } else {
                for (idx, item) in self.items.iter().enumerate() {
                    if item.category == category {
                        self.filtered_indices.push(idx as u32);
                    }
                }
            }
            return;
        }

        let mut query_lower = String::with_capacity(query_trimmed.len());
        push_lowercase(&mut query_lower, query_trimmed);
        let finder = Finder::new(query_lower.as_bytes());

        for (idx, item) in self.items.iter().enumerate() {
            if has_category && item.category != category {
                continue;
            }
            if finder.find(item.search_haystack.as_bytes()).is_some() {
                self.filtered_indices.push(idx as u32);
            }
        }
    }

    /// Met à jour la catégorie en mémoire sans recharger toute la base SQLite.
    pub fn set_category_in_memory(
        &mut self,
        paths: &[String],
        new_category: &str,
        query: &str,
        selected_category: &str,
    ) {
        let path_set: FxHashSet<&str> = paths.iter().map(String::as_str).collect();
        for item in &mut self.items {
            if path_set.contains(item.path.as_str()) {
                item.update_category(new_category);
            }
        }
        self.filter(query, selected_category);
    }

    /// Supprime une catégorie en mémoire sans recharger toute la base SQLite.
    pub fn clear_category_in_memory(
        &mut self,
        deleted_category: &str,
        query: &str,
        selected_category: &str,
    ) {
        for item in &mut self.items {
            if item.category == deleted_category {
                item.update_category("");
            }
        }
        self.filter(query, selected_category);
    }

    #[inline]
    pub fn total_count(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn filtered_count(&self) -> usize {
        self.filtered_indices.len()
    }

    #[inline]
    pub fn get_filtered(&self, row: usize) -> Option<&IndexItem> {
        let idx = *self.filtered_indices.get(row)? as usize;
        self.items.get(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{LibraryDatabase, ScannedEntry};

    #[test]
    fn test_sqlite_and_simd_index() {
        let temp_db = std::env::temp_dir().join("bibliotheque_test_unit.db");
        let _ = std::fs::remove_file(&temp_db);

        let db = LibraryDatabase::open_at(&temp_db).expect("open temp db");
        db.add_category("Jeux").expect("add category");
        db.add_category("Bureautique").expect("add category 2");

        let entries = vec![
            ScannedEntry {
                path: r"C:\Games\HollowKnight.exe".to_string(),
                name: "HollowKnight".to_string(),
                kind: ItemKind::Application,
            },
            ScannedEntry {
                path: r"C:\Users\Test\Desktop\Word.lnk".to_string(),
                name: "Word".to_string(),
                kind: ItemKind::Shortcut,
            },
        ];
        db.add_paths(&entries).expect("add paths");
        db.set_category(&[r"C:\Games\HollowKnight.exe".to_string()], "Jeux")
            .expect("set category");

        let records = db.load_all_items().expect("load items");
        assert_eq!(records.len(), 2);

        let mut index = InMemoryIndex::new();
        index.replace_all(records, "", "");
        assert_eq!(index.total_count(), 2);
        assert_eq!(index.filtered_count(), 2);

        index.filter("hollow", "");
        assert_eq!(index.filtered_count(), 1);
        assert_eq!(index.get_filtered(0).unwrap().name, "HollowKnight");

        index.filter("", "Jeux");
        assert_eq!(index.filtered_count(), 1);

        db.delete_category("Jeux").expect("delete category");
        index.clear_category_in_memory("Jeux", "", "Jeux");
        assert_eq!(index.filtered_count(), 0);

        drop(db);
        let _ = std::fs::remove_file(&temp_db);
    }
}
