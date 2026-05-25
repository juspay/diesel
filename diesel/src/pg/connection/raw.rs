#![allow(clippy::too_many_arguments)]
#![allow(unsafe_code)] // ffi code

extern crate pq_sys;

use self::pq_sys::*;
use alloc::ffi::CString;
use core::ffi as libc;
use core::ffi::CStr;
use core::ptr::NonNull;
use core::{ptr, str};

use crate::result::*;

use super::result::PgResult;
use crate::pg::PgNotification;

/// Builds connection parameters from a database URL, injecting `timezone=UTC`
/// and `client_encoding=UTF8` as connection parameters (via startup packet)
/// instead of using post-connection SET commands.
///
/// This prevents AWS RDS Proxy from pinning connections, because startup
/// parameters are not treated as session-mutating operations — only explicit
/// SET commands issued after connection cause pinning.
///
/// Uses `PQconninfoParse` (libpq's own parser) so that all valid connection
/// string formats (URI, key=value, etc.) are handled correctly without fragile
/// string manipulation.
fn build_connection_params(database_url: &str) -> ConnectionResult<(Vec<CString>, Vec<CString>)> {
    let connection_string = CString::new(database_url)?;

    let mut errmsg: *mut libc::c_char = ptr::null_mut();
    let conninfo = unsafe { PQconninfoParse(connection_string.as_ptr(), &mut errmsg) };

    if conninfo.is_null() {
        let message = if !errmsg.is_null() {
            let msg = unsafe { CStr::from_ptr(errmsg).to_string_lossy().into_owned() };
            unsafe { pq_sys::PQfreemem(errmsg as *mut libc::c_void) };
            msg
        } else {
            "Failed to parse connection string".to_string()
        };
        return Err(ConnectionError::BadConnection(message));
    }

    let mut keywords: Vec<CString> = Vec::new();
    let mut values: Vec<CString> = Vec::new();
    let mut has_client_encoding = false;
    let mut has_timezone_in_options = false;
    let mut options_idx: Option<usize> = None;

    // Iterate through PQconninfoOption array (terminated by null keyword)
    let mut opt = conninfo;
    while !unsafe { (*opt).keyword }.is_null() {
        let keyword_ptr = unsafe { (*opt).keyword };
        let val_ptr = unsafe { (*opt).val };

        let keyword = unsafe { CStr::from_ptr(keyword_ptr) }
            .to_string_lossy()
            .into_owned();

        if !val_ptr.is_null() {
            let val = unsafe { CStr::from_ptr(val_ptr) }
                .to_string_lossy()
                .into_owned();

            // Track whether client_encoding is already explicitly set
            if keyword == "client_encoding" {
                has_client_encoding = true;
            }

            // Track whether timezone is already set via options
            if keyword == "options" {
                options_idx = Some(keywords.len());
                if val.contains("timezone=") || val.contains("timezone =") {
                    has_timezone_in_options = true;
                }
            }

            keywords
                .push(CString::new(keyword).map_err(|e| {
                    ConnectionError::BadConnection(format!("Invalid keyword: {e}"))
                })?);
            values.push(
                CString::new(val)
                    .map_err(|e| ConnectionError::BadConnection(format!("Invalid value: {e}")))?,
            );
        }

        opt = unsafe { opt.add(1) };
    }

    // Free the PQconninfoParse result
    unsafe { PQconninfoFree(conninfo) };

    // Inject client_encoding if not already set
    if !has_client_encoding {
        keywords.push(CString::new("client_encoding").expect("valid CString"));
        values.push(CString::new("UTF8").expect("valid CString"));
    }

    // Inject timezone via options if not already set
    if !has_timezone_in_options {
        let timezone_opt = CString::new("-c timezone=UTC").expect("valid CString");
        if let Some(idx) = options_idx {
            // Append to existing options value
            let existing_val = values[idx].to_string_lossy().into_owned();
            let new_val = format!("{} {}", existing_val, timezone_opt.to_str().unwrap());
            values[idx] = CString::new(new_val).expect("valid CString");
        } else {
            // Add new options parameter
            keywords.push(CString::new("options").expect("valid CString"));
            values.push(timezone_opt);
        }
    }

    Ok((keywords, values))
}

#[allow(missing_debug_implementations, missing_copy_implementations)]
pub(super) struct RawConnection {
    pub(super) internal_connection: NonNull<PGconn>,
}

impl RawConnection {
    pub(super) fn establish(database_url: &str) -> ConnectionResult<Self> {
        let (keywords, values) = build_connection_params(database_url)?;

        // Build null-terminated pointer arrays for PQconnectdbParams
        let keyword_ptrs: Vec<*const libc::c_char> = keywords
            .iter()
            .map(|k| k.as_ptr())
            .chain(std::iter::once(ptr::null()))
            .collect();
        let value_ptrs: Vec<*const libc::c_char> = values
            .iter()
            .map(|v| v.as_ptr())
            .chain(std::iter::once(ptr::null()))
            .collect();

        // expand_dbname = 0 because PQconninfoParse already expanded the
        // connection string into individual parameters
        let connection_ptr =
            unsafe { PQconnectdbParams(keyword_ptrs.as_ptr(), value_ptrs.as_ptr(), 0) };
        let connection_status = unsafe { PQstatus(connection_ptr) };

        match connection_status {
            ConnStatusType::CONNECTION_OK => {
                let connection_ptr = unsafe { NonNull::new_unchecked(connection_ptr) };
                Ok(RawConnection {
                    internal_connection: connection_ptr,
                })
            }
            _ => {
                let message = last_error_message(connection_ptr);

                if !connection_ptr.is_null() {
                    // Note that even if the server connection attempt fails (as indicated by PQstatus),
                    // the application should call PQfinish to free the memory used by the PGconn object.
                    // https://www.postgresql.org/docs/current/libpq-connect.html
                    unsafe { PQfinish(connection_ptr) }
                }

                Err(ConnectionError::BadConnection(message))
            }
        }
    }

    pub(super) fn last_error_message(&self) -> String {
        last_error_message(self.internal_connection.as_ptr())
    }

    pub(super) fn set_notice_processor(&self, notice_processor: NoticeProcessor) {
        unsafe {
            PQsetNoticeProcessor(
                self.internal_connection.as_ptr(),
                Some(notice_processor),
                ptr::null_mut(),
            );
        }
    }

    pub(super) unsafe fn exec(&self, query: *const libc::c_char) -> QueryResult<RawResult> {
        let result_ptr = unsafe { PQexec(self.internal_connection.as_ptr(), query) };
        RawResult::new(result_ptr, self)
    }

    /// Sends a query and parameters to the server without using the prepare/bind cycle.
    ///
    /// This method uses PQsendQueryParams which combines the prepare and bind steps
    /// and is more compatible with connection poolers like PgBouncer.
    pub(super) unsafe fn send_query_params(
        &self,
        query: *const libc::c_char,
        param_count: libc::c_int,
        param_types: *const Oid,
        param_values: *const *const libc::c_char,
        param_lengths: *const libc::c_int,
        param_formats: *const libc::c_int,
        result_format: libc::c_int,
    ) -> QueryResult<()> {
        let res = unsafe {
            PQsendQueryParams(
                self.internal_connection.as_ptr(),
                query,
                param_count,
                param_types,
                param_values,
                param_lengths,
                param_formats,
                result_format,
            )
        };
        if res == 1 {
            Ok(())
        } else {
            Err(Error::DatabaseError(
                DatabaseErrorKind::UnableToSendCommand,
                Box::new(self.last_error_message()),
            ))
        }
    }

    pub(super) unsafe fn send_query_prepared(
        &self,
        stmt_name: *const libc::c_char,
        param_count: libc::c_int,
        param_values: *const *const libc::c_char,
        param_lengths: *const libc::c_int,
        param_formats: *const libc::c_int,
        result_format: libc::c_int,
    ) -> QueryResult<()> {
        let res = unsafe {
            PQsendQueryPrepared(
                self.internal_connection.as_ptr(),
                stmt_name,
                param_count,
                param_values,
                param_lengths,
                param_formats,
                result_format,
            )
        };
        if res == 1 {
            Ok(())
        } else {
            Err(Error::DatabaseError(
                DatabaseErrorKind::UnableToSendCommand,
                Box::new(self.last_error_message()),
            ))
        }
    }

    pub(super) unsafe fn prepare(
        &self,
        stmt_name: *const libc::c_char,
        query: *const libc::c_char,
        param_count: libc::c_int,
        param_types: *const Oid,
    ) -> QueryResult<RawResult> {
        let ptr = unsafe {
            PQprepare(
                self.internal_connection.as_ptr(),
                stmt_name,
                query,
                param_count,
                param_types,
            )
        };
        RawResult::new(ptr, self)
    }

    /// This is reasonably inexpensive as it just accesses variables internal to the connection
    /// that are kept up to date by the `ReadyForQuery` messages from the PG server
    pub(super) fn transaction_status(&self) -> PgTransactionStatus {
        unsafe { PQtransactionStatus(self.internal_connection.as_ptr()) }.into()
    }

    pub(super) fn get_status(&self) -> ConnStatusType {
        unsafe { PQstatus(self.internal_connection.as_ptr()) }
    }

    pub(crate) fn get_next_result(&self) -> Result<Option<PgResult>, Error> {
        let res = unsafe { PQgetResult(self.internal_connection.as_ptr()) };
        if res.is_null() {
            Ok(None)
        } else {
            let raw = RawResult::new(res, self)?;
            Ok(Some(PgResult::new(raw, self)?))
        }
    }

    pub(crate) fn enable_row_by_row_mode(&self) -> QueryResult<()> {
        let res = unsafe { PQsetSingleRowMode(self.internal_connection.as_ptr()) };
        if res == 1 {
            Ok(())
        } else {
            Err(Error::DatabaseError(
                DatabaseErrorKind::Unknown,
                Box::new(self.last_error_message()),
            ))
        }
    }

    pub(super) fn put_copy_data(&mut self, buf: &[u8]) -> QueryResult<()> {
        for c in buf.chunks(i32::MAX as usize) {
            let res = unsafe {
                pq_sys::PQputCopyData(
                    self.internal_connection.as_ptr(),
                    c.as_ptr() as *const libc::c_char,
                    c.len()
                        .try_into()
                        .map_err(|e| Error::SerializationError(Box::new(e)))?,
                )
            };
            if res != 1 {
                return Err(Error::DatabaseError(
                    DatabaseErrorKind::Unknown,
                    Box::new(self.last_error_message()),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn finish_copy_from(&self, err: Option<String>) -> QueryResult<()> {
        let error = err.map(CString::new).map(|r| {
            r.unwrap_or_else(|_| {
                CString::new("Error message contains a \\0 byte")
                    .expect("Does not contain a null byte")
            })
        });
        let error = error
            .as_ref()
            .map(|l| l.as_ptr())
            .unwrap_or(core::ptr::null());
        let ret = unsafe { pq_sys::PQputCopyEnd(self.internal_connection.as_ptr(), error) };
        if ret == 1 {
            Ok(())
        } else {
            Err(Error::DatabaseError(
                DatabaseErrorKind::Unknown,
                Box::new(self.last_error_message()),
            ))
        }
    }

    pub(super) fn pq_notifies(&self) -> Result<Option<PgNotification>, Error> {
        let conn = self.internal_connection;
        let ret = unsafe { PQconsumeInput(conn.as_ptr()) };
        if ret == 0 {
            return Err(Error::DatabaseError(
                DatabaseErrorKind::Unknown,
                Box::new(self.last_error_message()),
            ));
        }

        let pgnotify = unsafe { PQnotifies(conn.as_ptr()) };
        if pgnotify.is_null() {
            Ok(None)
        } else {
            // we use a drop guard here to
            // make sure that we always free
            // the provided pointer, even if we
            // somehow return an error below
            struct Guard<'a> {
                value: &'a mut pgNotify,
            }

            impl Drop for Guard<'_> {
                fn drop(&mut self) {
                    unsafe {
                        // SAFETY: We know that this value is not null here
                        PQfreemem(self.value as *mut pgNotify as *mut core::ffi::c_void)
                    };
                }
            }

            let pgnotify = unsafe {
                // SAFETY: We checked for null values above
                Guard {
                    value: &mut *pgnotify,
                }
            };
            if pgnotify.value.relname.is_null() {
                return Err(Error::DeserializationError(
                    "Received an unexpected null value for `relname` from the notification".into(),
                ));
            }
            if pgnotify.value.extra.is_null() {
                return Err(Error::DeserializationError(
                    "Received an unexpected null value for `extra` from the notification".into(),
                ));
            }

            let channel = unsafe {
                // SAFETY: We checked for null values above
                CStr::from_ptr(pgnotify.value.relname)
            }
            .to_str()
            .map_err(|e| Error::DeserializationError(e.into()))?
            .to_string();
            let payload = unsafe {
                // SAFETY: We checked for null values above
                CStr::from_ptr(pgnotify.value.extra)
            }
            .to_str()
            .map_err(|e| Error::DeserializationError(e.into()))?
            .to_string();
            let ret = PgNotification {
                process_id: pgnotify.value.be_pid,
                channel,
                payload,
            };
            Ok(Some(ret))
        }
    }
}

/// Represents the current in-transaction status of the connection
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum PgTransactionStatus {
    /// Currently idle
    Idle,
    /// A command is in progress (sent to the server but not yet completed)
    Active,
    /// Idle, in a valid transaction block
    InTransaction,
    /// Idle, in a failed transaction block
    InError,
    /// Bad connection
    Unknown,
}

impl From<PGTransactionStatusType> for PgTransactionStatus {
    fn from(trans_status_type: PGTransactionStatusType) -> Self {
        match trans_status_type {
            PGTransactionStatusType::PQTRANS_IDLE => PgTransactionStatus::Idle,
            PGTransactionStatusType::PQTRANS_ACTIVE => PgTransactionStatus::Active,
            PGTransactionStatusType::PQTRANS_INTRANS => PgTransactionStatus::InTransaction,
            PGTransactionStatusType::PQTRANS_INERROR => PgTransactionStatus::InError,
            PGTransactionStatusType::PQTRANS_UNKNOWN => PgTransactionStatus::Unknown,
        }
    }
}

pub(super) type NoticeProcessor =
    extern "C" fn(arg: *mut libc::c_void, message: *const libc::c_char);

impl Drop for RawConnection {
    fn drop(&mut self) {
        unsafe { PQfinish(self.internal_connection.as_ptr()) };
    }
}

fn last_error_message(conn: *const PGconn) -> String {
    unsafe {
        let error_ptr = PQerrorMessage(conn);
        let bytes = CStr::from_ptr(error_ptr).to_bytes();
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// Internal wrapper around a `*mut PGresult` which is known to be not-null, and
/// have no aliases.  This wrapper is to ensure that it's always properly
/// dropped.
///
/// If `Unique` is ever stabilized, we should use it here.
#[allow(missing_debug_implementations)]
pub(super) struct RawResult(NonNull<PGresult>);

// SAFETY:
// https://www.postgresql.org/docs/current/libpq-threading.html
//
// PGresult objects are normally read-only after creation,
// and so can be passed around freely between threads. However,
// if you use any of the PGresult-modifying functions described in
// Section 31.12 or Section 31.14, it's up to you to avoid concurrent operations on the same PGresult, too.
//
// The type doesn't expose the raw pointer
// and we don't call such Pgresult-modifying below
unsafe impl Send for RawResult {}
unsafe impl Sync for RawResult {}

// Make sure to only call non-modifying functions here
impl RawResult {
    #[allow(clippy::new_ret_no_self)]
    fn new(ptr: *mut PGresult, conn: &RawConnection) -> QueryResult<Self> {
        NonNull::new(ptr).map(RawResult).ok_or_else(|| {
            Error::DatabaseError(
                DatabaseErrorKind::UnableToSendCommand,
                Box::new(conn.last_error_message()),
            )
        })
    }

    pub(super) fn error_message(&self) -> &str {
        let ptr = unsafe { PQresultErrorMessage(self.0.as_ptr()) };
        let cstr = unsafe { CStr::from_ptr(ptr) };
        cstr.to_str().unwrap_or_default()
    }

    pub(super) fn get_result_field(&self, field: ResultField) -> Option<&str> {
        let ptr = unsafe { PQresultErrorField(self.0.as_ptr(), field as libc::c_int) };
        if ptr.is_null() {
            return None;
        }

        let c_str = unsafe { CStr::from_ptr(ptr) };
        c_str.to_str().ok()
    }

    pub(super) fn result_status(&self) -> pq_sys::ExecStatusType {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            PQresultStatus(self.0.as_ptr())
        }
    }

    pub(super) fn column_count(&self) -> libc::c_int {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            PQnfields(self.0.as_ptr())
        }
    }

    pub(super) fn row_count(&self) -> libc::c_int {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            PQntuples(self.0.as_ptr())
        }
    }

    pub(super) fn rows_affected(&self) -> &CStr {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            let count_char_ptr = PQcmdTuples(self.0.as_ptr());
            CStr::from_ptr(count_char_ptr)
        }
    }

    pub(super) fn get_bytes(&self, row_idx: i32, col_idx: i32) -> &[u8] {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            let value_ptr = PQgetvalue(self.0.as_ptr(), row_idx, col_idx) as *const u8;
            let num_bytes = PQgetlength(self.0.as_ptr(), row_idx, col_idx);
            if value_ptr.is_null() {
                &[]
            } else {
                // SAFETY:
                // * we rely on correct information from libpq here, if the provided length and pointer are invalid
                core::slice::from_raw_parts(
                    value_ptr,
                    num_bytes
                        .try_into()
                        .expect("Diesel expects at least a 32 bit operating system"),
                )
            }
        }
    }

    pub(super) fn is_null(&self, row_idx: libc::c_int, col_idx: libc::c_int) -> libc::c_int {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            PQgetisnull(self.0.as_ptr(), row_idx, col_idx)
        }
    }

    pub(super) fn column_type(&self, col_idx: libc::c_int) -> pq_sys::Oid {
        unsafe {
            // SAFETY:
            // We have a unique not null pointer here
            PQftype(self.0.as_ptr(), col_idx)
        }
    }

    pub(super) fn column_name(&self, col_idx: libc::c_int) -> Option<&CStr> {
        unsafe {
            // https://www.postgresql.org/docs/13/libpq-exec.html#LIBPQ-PQFNAME
            // states that the returned ptr is valid till the underlying result is freed
            // That means we can couple the lifetime to self
            let ptr = PQfname(self.0.as_ptr(), col_idx);
            if ptr.is_null() {
                None
            } else {
                Some(CStr::from_ptr(ptr))
            }
        }
    }
}

impl Drop for RawResult {
    fn drop(&mut self) {
        unsafe { PQclear(self.0.as_ptr()) }
    }
}

/// Represents valid options to
/// [`PQresultErrorField`](https://www.postgresql.org/docs/current/static/libpq-exec.html#LIBPQ-PQRESULTERRORFIELD)
/// Their values are defined as C preprocessor macros, and therefore are not exported by libpq-sys.
/// Their values can be found in `postgres_ext.h`
#[repr(i32)]
pub(super) enum ResultField {
    SqlState = 'C' as i32,
    MessagePrimary = 'M' as i32,
    MessageDetail = 'D' as i32,
    MessageHint = 'H' as i32,
    TableName = 't' as i32,
    ColumnName = 'c' as i32,
    ConstraintName = 'n' as i32,
    StatementPosition = 'P' as i32,
}
