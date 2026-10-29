use std::ffi::{CStr, CString, c_void};
use std::path::Path;
use std::ptr;

use crate::win_utils::{database_path, now_iso8601};

#[repr(C)]
pub struct Sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct Sqlite3Stmt {
    _private: [u8; 0],
}

const SQLITE_OK: i32 = 0;
const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;
const SQLITE_OPEN_READWRITE: i32 = 0x0000_0002;
const SQLITE_OPEN_CREATE: i32 = 0x0000_0004;
const SQLITE_OPEN_NOMUTEX: i32 = 0x0000_8000;
const SQLITE_STATIC: isize = 0;

#[link(name = "winsqlite3", kind = "raw-dylib")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const u8,
        pp_db: *mut *mut Sqlite3,
        flags: i32,
        z_vfs: *const u8,
    ) -> i32;
    fn sqlite3_close_v2(db: *mut Sqlite3) -> i32;
    fn sqlite3_busy_timeout(db: *mut Sqlite3, ms: i32) -> i32;
    fn sqlite3_exec(
        db: *mut Sqlite3,
        sql: *const u8,
        callback: *const c_void,
        arg: *mut c_void,
        errmsg: *mut *mut u8,
    ) -> i32;
    fn sqlite3_prepare_v2(
        db: *mut Sqlite3,
        z_sql: *const u8,
        n_byte: i32,
        pp_stmt: *mut *mut Sqlite3Stmt,
        pz_tail: *mut *const u8,
    ) -> i32;
    fn sqlite3_step(stmt: *mut Sqlite3Stmt) -> i32;
    fn sqlite3_reset(stmt: *mut Sqlite3Stmt) -> i32;
    fn sqlite3_clear_bindings(stmt: *mut Sqlite3Stmt) -> i32;
    fn sqlite3_finalize(stmt: *mut Sqlite3Stmt) -> i32;
    fn sqlite3_bind_text(
        stmt: *mut Sqlite3Stmt,
        index: i32,
        value: *const u8,
        n_byte: i32,
        destructor: isize,
    ) -> i32;
    fn sqlite3_column_text(stmt: *mut Sqlite3Stmt, i_col: i32) -> *const u8;
    fn sqlite3_column_bytes(stmt: *mut Sqlite3Stmt, i_col: i32) -> i32;
    fn sqlite3_errmsg(db: *mut Sqlite3) -> *const u8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Application,
    Shortcut,
}

impl ItemKind {
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "Application",
            Self::Shortcut => "Raccourci d'application",
        }
    }

    #[inline]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "Application" => Some(Self::Application),
            "Raccourci d'application" => Some(Self::Shortcut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScannedEntry {
    pub path: String,
    pub name: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone)]
pub struct DbRecord {
    pub name: String,
    pub kind: ItemKind,
    pub category: String,
    pub path: String,
}

pub struct LibraryDatabase {
    db: *mut Sqlite3,
}

unsafe impl Send for LibraryDatabase {}

impl LibraryDatabase {
    pub fn open() -> Result<Self, String> {
        Self::open_at(&database_path())
    }

    pub fn open_at(path: &Path) -> Result<Self, String> {
        let path_str = path.to_string_lossy();
        let c_path = CString::new(path_str.as_bytes()).map_err(|e| e.to_string())?;
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let rc = unsafe {
            sqlite3_open_v2(
                c_path.as_ptr().cast(),
                &mut db,
                SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_NOMUTEX,
                ptr::null(),
            )
        };
        if rc != SQLITE_OK || db.is_null() {
            let msg = Self::last_error_raw(db);
            if !db.is_null() {
                unsafe {
                    sqlite3_close_v2(db);
                }
            }
            return Err(format!("Ouverture SQLite échouée: {msg}"));
        }

        unsafe {
            sqlite3_busy_timeout(db, 5000);
        }

        let instance = Self { db };
        instance.init_schema()?;
        Ok(instance)
    }

    fn init_schema(&self) -> Result<(), String> {
        // Optimisations SQLite maximales (WAL, mémoire partagée, synchronisation normale)
        self.exec_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA temp_store = MEMORY;\
             PRAGMA cache_size = -16000;\
             PRAGMA mmap_size = 268435456;\
             CREATE TABLE IF NOT EXISTS items (\
                 path TEXT PRIMARY KEY,\
                 name TEXT NOT NULL,\
                 kind TEXT NOT NULL,\
                 category TEXT NOT NULL DEFAULT '',\
                 updated_at TEXT NOT NULL\
             );\
             CREATE TABLE IF NOT EXISTS categories (\
                 name TEXT PRIMARY KEY\
             );\
             CREATE INDEX IF NOT EXISTS idx_items_category ON items(category);\
             CREATE INDEX IF NOT EXISTS idx_items_name_nocase ON items(name COLLATE NOCASE);\
             DELETE FROM items WHERE kind NOT IN ('Application', 'Raccourci d''application');\0",
        )
    }

    fn exec_batch(&self, sql_with_nul: &str) -> Result<(), String> {
        let rc = unsafe {
            sqlite3_exec(
                self.db,
                sql_with_nul.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            Err(self.last_error())
        } else {
            Ok(())
        }
    }

    fn last_error(&self) -> String {
        Self::last_error_raw(self.db)
    }

    fn last_error_raw(db: *mut Sqlite3) -> String {
        if db.is_null() {
            return "Pointeur SQLite nul".to_string();
        }
        unsafe {
            let ptr = sqlite3_errmsg(db);
            if ptr.is_null() {
                "Erreur SQLite inconnue".to_string()
            } else {
                CStr::from_ptr(ptr.cast()).to_string_lossy().into_owned()
            }
        }
    }

    #[inline]
    unsafe fn column_str(stmt: *mut Sqlite3Stmt, col: i32) -> String {
        unsafe {
            let ptr = sqlite3_column_text(stmt, col);
            let len = sqlite3_column_bytes(stmt, col);
            if ptr.is_null() || len <= 0 {
                String::new()
            } else {
                let slice = std::slice::from_raw_parts(ptr, len as usize);
                String::from_utf8_lossy(slice).into_owned()
            }
        }
    }

    #[inline]
    unsafe fn bind_str(stmt: *mut Sqlite3Stmt, idx: i32, val: &str) -> i32 {
        unsafe {
            sqlite3_bind_text(
                stmt,
                idx,
                val.as_ptr(),
                val.len() as i32,
                SQLITE_STATIC,
            )
        }
    }

    /// Insère ou met à jour un lot d'éléments en une seule transaction atomique préparée.
    pub fn add_paths(&self, entries: &[ScannedEntry]) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        let now = now_iso8601();
        self.exec_batch("BEGIN IMMEDIATE TRANSACTION;\0")?;

        const UPSERT_SQL: &str = "INSERT INTO items(path, name, kind, updated_at) VALUES(?1, ?2, ?3, ?4) \
             ON CONFLICT(path) DO UPDATE SET name=excluded.name, kind=excluded.kind, updated_at=excluded.updated_at;\0";

        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.db,
                UPSERT_SQL.as_ptr(),
                (UPSERT_SQL.len() - 1) as i32,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            let _ = self.exec_batch("ROLLBACK;\0");
            return Err(self.last_error());
        }

        for item in entries {
            let kind_str = item.kind.as_str();
            unsafe {
                Self::bind_str(stmt, 1, &item.path);
                Self::bind_str(stmt, 2, &item.name);
                Self::bind_str(stmt, 3, kind_str);
                Self::bind_str(stmt, 4, &now);
                let step_rc = sqlite3_step(stmt);
                sqlite3_reset(stmt);
                sqlite3_clear_bindings(stmt);
                if step_rc != SQLITE_DONE {
                    sqlite3_finalize(stmt);
                    let _ = self.exec_batch("ROLLBACK;\0");
                    return Err(self.last_error());
                }
            }
        }

        unsafe {
            sqlite3_finalize(stmt);
        }
        self.exec_batch("COMMIT;\0")
    }

    /// Charge tous les éléments en mémoire en une seule passe séquentielle triée.
    pub fn load_all_items(&self) -> Result<Vec<DbRecord>, String> {
        const SQL: &str =
            "SELECT name, kind, category, path FROM items ORDER BY name COLLATE NOCASE;\0";
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.db,
                SQL.as_ptr(),
                (SQL.len() - 1) as i32,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            return Err(self.last_error());
        }

        let mut records = Vec::with_capacity(2048);
        unsafe {
            while sqlite3_step(stmt) == SQLITE_ROW {
                let name = Self::column_str(stmt, 0);
                let kind_raw = Self::column_str(stmt, 1);
                if let Some(kind) = ItemKind::from_db_str(&kind_raw) {
                    let category = Self::column_str(stmt, 2);
                    let path = Self::column_str(stmt, 3);
                    records.push(DbRecord {
                        name,
                        kind,
                        category,
                        path,
                    });
                }
            }
            sqlite3_finalize(stmt);
        }
        Ok(records)
    }

    pub fn categories(&self) -> Vec<String> {
        const SQL: &str = "SELECT name FROM categories ORDER BY name COLLATE NOCASE;\0";
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.db,
                SQL.as_ptr(),
                (SQL.len() - 1) as i32,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            return Vec::new();
        }

        let mut list = Vec::new();
        unsafe {
            while sqlite3_step(stmt) == SQLITE_ROW {
                let name = Self::column_str(stmt, 0);
                if !name.is_empty() {
                    list.push(name);
                }
            }
            sqlite3_finalize(stmt);
        }
        list
    }

    pub fn add_category(&self, name: &str) -> Result<(), String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        const SQL: &str = "INSERT OR IGNORE INTO categories(name) VALUES(?1);\0";
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.db,
                SQL.as_ptr(),
                (SQL.len() - 1) as i32,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            return Err(self.last_error());
        }
        unsafe {
            Self::bind_str(stmt, 1, trimmed);
            let step_rc = sqlite3_step(stmt);
            sqlite3_finalize(stmt);
            if step_rc != SQLITE_DONE {
                return Err(self.last_error());
            }
        }
        Ok(())
    }

    pub fn delete_category(&self, name: &str) -> Result<(), String> {
        self.exec_batch("BEGIN IMMEDIATE TRANSACTION;\0")?;

        const UPDATE_SQL: &str = "UPDATE items SET category='' WHERE category=?1;\0";
        const DELETE_SQL: &str = "DELETE FROM categories WHERE name=?1;\0";

        for sql in [UPDATE_SQL, DELETE_SQL] {
            let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
            let rc = unsafe {
                sqlite3_prepare_v2(
                    self.db,
                    sql.as_ptr(),
                    (sql.len() - 1) as i32,
                    &mut stmt,
                    ptr::null_mut(),
                )
            };
            if rc != SQLITE_OK || stmt.is_null() {
                let _ = self.exec_batch("ROLLBACK;\0");
                return Err(self.last_error());
            }
            unsafe {
                Self::bind_str(stmt, 1, name);
                let step_rc = sqlite3_step(stmt);
                sqlite3_finalize(stmt);
                if step_rc != SQLITE_DONE {
                    let _ = self.exec_batch("ROLLBACK;\0");
                    return Err(self.last_error());
                }
            }
        }

        self.exec_batch("COMMIT;\0")
    }

    pub fn set_category(&self, paths: &[String], category: &str) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        self.exec_batch("BEGIN IMMEDIATE TRANSACTION;\0")?;
        const SQL: &str = "UPDATE items SET category=?1 WHERE path=?2;\0";
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.db,
                SQL.as_ptr(),
                (SQL.len() - 1) as i32,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            let _ = self.exec_batch("ROLLBACK;\0");
            return Err(self.last_error());
        }

        for path in paths {
            unsafe {
                Self::bind_str(stmt, 1, category);
                Self::bind_str(stmt, 2, path);
                let step_rc = sqlite3_step(stmt);
                sqlite3_reset(stmt);
                sqlite3_clear_bindings(stmt);
                if step_rc != SQLITE_DONE {
                    sqlite3_finalize(stmt);
                    let _ = self.exec_batch("ROLLBACK;\0");
                    return Err(self.last_error());
                }
            }
        }

        unsafe {
            sqlite3_finalize(stmt);
        }
        self.exec_batch("COMMIT;\0")
    }
}

impl Drop for LibraryDatabase {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe {
                sqlite3_close_v2(self.db);
            }
            self.db = ptr::null_mut();
        }
    }
}
