use std::sync::OnceLock;

const AUTO_ENABLE_ENV: &str = "BONES_SQLITE_VEC_AUTO";

static REGISTRATION: OnceLock<Result<(), String>> = OnceLock::new();

pub fn register_auto_extension() -> Result<(), String> {
    if matches!(
        std::env::var(AUTO_ENABLE_ENV).ok().as_deref(),
        Some("0" | "false" | "off")
    ) {
        return Err(format!(
            "sqlite-vec auto-extension disabled by {}",
            AUTO_ENABLE_ENV
        ));
    }

    REGISTRATION.get_or_init(register_once).clone()
}

fn register_once() -> Result<(), String> {
    #[allow(clippy::transmute_ptr_to_ptr)]
    let entrypoint: unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *const std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int =
        unsafe { std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ()) };

    let rc = unsafe { rusqlite::ffi::sqlite3_auto_extension(Some(entrypoint)) };
    if rc == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(format!("sqlite3_auto_extension failed with rc={rc}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn registration_makes_vec_version_available() {
        let result = register_auto_extension();
        assert!(result.is_ok(), "registration failed: {result:?}");

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        let version = conn.query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0));
        assert!(
            version.is_ok(),
            "vec_version() should be available after registration"
        );
    }
}
