//! remember-mcp — MCP server for semantic search over local OpenCode chat history.
//!
//! Read-only RAG index over OpenCode's SQLite conversation store plus the
//! legacy JSON storage tree. Serves MCP over stdio as a child process of the
//! client (OpenCode), so it starts and stops with it.

pub mod chunking;
pub mod config;
pub mod embed;
pub mod index;
pub mod redact;
pub mod search;
pub mod server;
pub mod sources;
pub mod transcript;
pub mod util;

/// Register the sqlite-vec extension process-wide (idempotent).
/// Must be called before opening any connection to `index.db`.
pub fn register_sqlite_vec() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let init: unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
}
