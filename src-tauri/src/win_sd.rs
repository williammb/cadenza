//! Windows security-descriptor helpers for the IPC/auth boundary.
//!
//! Two surfaces share one policy: "only the current user may access this".
//!
//! - The CLI auth token file (`~/.cadenza/auth`) gets a restrictive DACL so
//!   other local users can't read the token (Unix uses `mode(0o600)`; this is
//!   the Windows equivalent — see [`apply_owner_only_dacl`]).
//! - The named pipe (`cadenza-<user>`) gets the same descriptor so only the
//!   current user may connect (see [`current_user_security_descriptor`]).
//!
//! The DACL is expressed as an [SDDL] string
//! `D:P(A;;FA;;;<SID>)` — a **P**rotected DACL (no inherited ACEs) with a
//! single **A**llow ACE granting **F**ull **A**ccess to the current user's SID
//! and nobody else. The SID is resolved at runtime from the process token. The
//! string-building and SID-validation logic is split into pure functions so it
//! is unit-testable on every platform, while the Win32 FFI is `cfg(windows)`.
//!
//! [SDDL]: https://learn.microsoft.com/en-us/windows/win32/secauthz/security-descriptor-string-format

#![cfg_attr(not(windows), allow(dead_code))]

/// Build the SDDL string for an owner-only DACL granting full access to a
/// single SID and no inherited ACEs.
///
/// Pure and platform-independent so the construction is unit-testable without a
/// live Windows token. `sid` must be a valid SDDL SID string (a numeric SID
/// like `S-1-5-21-...` or a 2-letter well-known alias like `BA`); callers
/// resolve it from the process token via [`current_user_sid_string`].
pub fn owner_only_dacl_sddl(sid: &str) -> String {
    // D:  -> DACL section
    // P   -> SE_DACL_PROTECTED: drop inherited ACEs (no parent-folder access)
    // (A;;FA;;;<sid>) -> Allow ACE, FILE_ALL_ACCESS / full access, for <sid>
    format!("D:P(A;;FA;;;{sid})")
}

/// `true` if `sid` is a syntactically plausible SDDL SID token: either a
/// well-known 2-letter alias (e.g. `BA`, `SY`) or a numeric `S-1-...` SID.
///
/// This is a defensive guard against feeding an unexpected token string into
/// the SDDL builder; it is intentionally conservative rather than a full SDDL
/// validator (the OS makes the final call when parsing the descriptor).
pub fn is_plausible_sddl_sid(sid: &str) -> bool {
    if sid.len() == 2 && sid.bytes().all(|b| b.is_ascii_uppercase()) {
        // Well-known SID alias such as BA (Builtin Admins) or SY (Local System).
        return true;
    }
    // Numeric SID: S-1-<authority>-<sub-authority>...
    let mut parts = sid.split('-');
    if parts.next() != Some("S") {
        return false;
    }
    if parts.next() != Some("1") {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    !rest.is_empty()
        && rest
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use widestring::{U16CStr, U16CString};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SetFileSecurityW, TokenUser, DACL_SECURITY_INFORMATION, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// RAII wrapper that frees a `LocalAlloc`-backed buffer with `LocalFree`.
    struct LocalBuf(*mut core::ffi::c_void);
    impl Drop for LocalBuf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    /// RAII wrapper for a process-token `HANDLE`.
    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// Resolve the current process user's SID as an SDDL string (`S-1-5-21-...`).
    pub fn current_user_sid_string() -> io::Result<String> {
        unsafe {
            // Open the current process token for query.
            let mut raw_token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = TokenHandle(raw_token);

            // Two-call pattern: first ask for the required buffer length.
            let mut needed: u32 = 0;
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return Err(io::Error::last_os_error());
            }

            // Allocate and fetch the TOKEN_USER blob (TOKEN_USER + trailing SID).
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }

            // SAFETY: the buffer was filled by GetTokenInformation(TokenUser).
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            let sid = token_user.User.Sid;
            if sid.is_null() {
                return Err(io::Error::other("null SID in token"));
            }

            // Convert the binary SID to its SDDL string form.
            let mut sid_str_ptr: windows_sys::core::PWSTR = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut sid_str_ptr) == 0 {
                return Err(io::Error::last_os_error());
            }
            let _guard = LocalBuf(sid_str_ptr.cast());
            if sid_str_ptr.is_null() {
                return Err(io::Error::other("null SID string"));
            }
            let sid_string = U16CStr::from_ptr_str(sid_str_ptr).to_string_lossy();
            Ok(sid_string)
        }
    }

    /// Build the owner-only SDDL string for the current user.
    pub fn current_user_sddl() -> io::Result<String> {
        let sid = current_user_sid_string()?;
        if !is_plausible_sddl_sid(&sid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resolved SID is not a plausible SDDL SID: {sid}"),
            ));
        }
        Ok(owner_only_dacl_sddl(&sid))
    }

    /// Build an interprocess `SecurityDescriptor` restricting the named pipe to
    /// the current user. Returned by-value for `ListenerOptions`.
    pub fn current_user_security_descriptor(
    ) -> io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
        let sddl = current_user_sddl()?;
        let wide = U16CString::from_str(&sddl)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        interprocess::os::windows::security_descriptor::SecurityDescriptor::deserialize(&wide)
    }

    /// Apply the owner-only DACL to `path`. Idempotent: callers invoke it on
    /// every write/rotation, not only first creation.
    pub fn apply_owner_only_dacl(path: &Path) -> io::Result<()> {
        let sddl = current_user_sddl()?;
        let sddl_wide = U16CString::from_str(&sddl)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        unsafe {
            // Convert the SDDL string into a self-relative security descriptor.
            let mut psd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _guard = LocalBuf(psd);

            // Apply only the DACL — leave owner/group/SACL untouched.
            let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            if SetFileSecurityW(path_wide.as_ptr(), DACL_SECURITY_INFORMATION, psd) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

// Only these two are consumed by non-test code (auth.rs / ipc.rs). The other
// `imp` helpers are exercised through the in-module tests via the `imp::` path.
#[cfg(windows)]
pub use imp::{apply_owner_only_dacl, current_user_security_descriptor};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_is_protected_owner_only_full_access() {
        let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let sddl = owner_only_dacl_sddl(sid);
        // Protected DACL (P) so no inherited ACEs leak access.
        assert!(sddl.starts_with("D:P("), "missing protected DACL: {sddl}");
        // Exactly one Allow ACE, full access, for our SID.
        assert_eq!(sddl, format!("D:P(A;;FA;;;{sid})"));
        // No second ACE → nobody else is granted anything.
        assert_eq!(sddl.matches("(A;").count(), 1);
        // The SID we asked for is present.
        assert!(sddl.contains(sid));
    }

    #[test]
    fn sddl_embeds_well_known_alias() {
        let sddl = owner_only_dacl_sddl("BA");
        assert_eq!(sddl, "D:P(A;;FA;;;BA)");
    }

    #[test]
    fn plausible_sid_accepts_numeric_and_aliases() {
        assert!(is_plausible_sddl_sid(
            "S-1-5-21-1111111111-2222222222-3333333333-1001"
        ));
        assert!(is_plausible_sddl_sid("S-1-5-18")); // Local System
        assert!(is_plausible_sddl_sid("BA")); // Builtin Admins alias
        assert!(is_plausible_sddl_sid("SY")); // Local System alias
    }

    #[test]
    fn plausible_sid_rejects_garbage() {
        assert!(!is_plausible_sddl_sid("")); // empty
        assert!(!is_plausible_sddl_sid("S-2-5-18")); // wrong revision
        assert!(!is_plausible_sddl_sid("X-1-5-18")); // not S-prefixed
        assert!(!is_plausible_sddl_sid("S-1-")); // no sub-authorities
        assert!(!is_plausible_sddl_sid("S-1-5-abc")); // non-numeric component
        assert!(!is_plausible_sddl_sid("ba")); // lowercase alias
        assert!(!is_plausible_sddl_sid("ADMIN")); // 5-letter, not an alias
                                                  // An injection attempt with an extra ACE must not pass the SID guard.
        assert!(!is_plausible_sddl_sid("S-1-1-0)(A;;FA;;;WD"));
    }

    // Windows-only: exercises the real token → SID → SDDL path. Cross-user
    // enforcement can't be unit-tested (it needs a second logon), but we can
    // assert the descriptor is built from a real, plausible current-user SID.
    #[cfg(windows)]
    #[test]
    fn current_user_sddl_is_owner_only_and_plausible() {
        let sid = imp::current_user_sid_string().expect("resolve current user SID");
        assert!(
            is_plausible_sddl_sid(&sid),
            "resolved SID not plausible: {sid}"
        );
        let sddl = imp::current_user_sddl().expect("build current-user SDDL");
        assert_eq!(sddl, format!("D:P(A;;FA;;;{sid})"));
        // The descriptor must actually parse on this OS.
        imp::current_user_security_descriptor().expect("build pipe security descriptor");
    }

    #[cfg(windows)]
    #[test]
    fn apply_dacl_to_file_succeeds_and_is_idempotent() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"token").unwrap();
        drop(f);
        // First application and a re-application (rotation) must both succeed.
        apply_owner_only_dacl(&path).expect("apply DACL once");
        apply_owner_only_dacl(&path).expect("re-apply DACL (idempotent)");
    }
}
