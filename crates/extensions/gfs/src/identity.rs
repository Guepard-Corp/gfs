use std::ffi::CString;

use pgrx::pg_sys;

use crate::catalog::spi_text;

const PINNED_SEARCH_PATH: &str = "pg_catalog, pg_temp";

pub(crate) struct AsRole {
    user: pg_sys::Oid,
    context: i32,
    nest: i32,
}

impl AsRole {
    unsafe fn enter(role: pg_sys::Oid, search_path: &str) -> Self {
        let (mut user, mut context) = (pg_sys::InvalidOid, 0);
        pg_sys::GetUserIdAndSecContext(&mut user, &mut context);
        pg_sys::SetUserIdAndSecContext(
            role,
            context
                | pg_sys::SECURITY_LOCAL_USERID_CHANGE as i32
                | pg_sys::SECURITY_RESTRICTED_OPERATION as i32,
        );
        let nest = pg_sys::NewGUCNestLevel();
        let name = CString::new("search_path").unwrap();
        let value = CString::new(search_path).unwrap();
        pg_sys::set_config_option(
            name.as_ptr(),
            value.as_ptr(),
            pg_sys::GucContext::PGC_USERSET,
            pg_sys::GucSource::PGC_S_SESSION,
            pg_sys::GucAction::GUC_ACTION_SAVE,
            true,
            pg_sys::PGERROR as i32,
            false,
        );
        Self { user, context, nest }
    }
}

impl Drop for AsRole {
    fn drop(&mut self) {
        unsafe {
            pg_sys::AtEOXact_GUC(false, self.nest);
            pg_sys::SetUserIdAndSecContext(self.user, self.context);
        }
    }
}

unsafe fn first_text(query: &str) -> Option<String> {
    let q = CString::new(query).ok()?;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32
        || pg_sys::SPI_processed != 1
    {
        return None;
    }
    let tt = pg_sys::SPI_tuptable;
    spi_text(pg_sys::SPI_getvalue(*(*tt).vals, (*tt).tupdesc, 1))
}

unsafe fn role_of(query: &str) -> Option<pg_sys::Oid> {
    first_text(query)
        .and_then(|text| text.trim().parse::<u32>().ok())
        .map(pg_sys::Oid::from)
}

// The reader may only SELECT: filling the local table is the owner's write, under a path the reader cannot plant in.
pub(crate) unsafe fn as_table_owner(relid: pg_sys::Oid) -> Option<AsRole> {
    let owner = role_of(&format!(
        "SELECT relowner::oid::int8::text FROM pg_catalog.pg_class WHERE oid = {}",
        u32::from(relid)
    ))?;
    let dblink_schema = first_text(
        "SELECT pg_catalog.quote_ident(n.nspname) FROM pg_catalog.pg_extension e \
           JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'dblink'",
    );
    let search_path = match dblink_schema {
        Some(schema) => format!("pg_catalog, {schema}, pg_temp"),
        None => PINNED_SEARCH_PATH.to_string(),
    };
    Some(AsRole::enter(owner, &search_path))
}

pub(crate) unsafe fn as_extension_owner() -> Option<AsRole> {
    let owner = role_of(
        "SELECT extowner::oid::int8::text FROM pg_catalog.pg_extension WHERE extname = 'gfs'",
    )?;
    Some(AsRole::enter(owner, PINNED_SEARCH_PATH))
}

pub(crate) unsafe fn execute_as_extension_owner(sql: &str) {
    let Ok(statement) = CString::new(sql) else {
        return;
    };
    let _as_owner = as_extension_owner();
    pg_sys::SPI_execute(statement.as_ptr(), false, 0);
}
