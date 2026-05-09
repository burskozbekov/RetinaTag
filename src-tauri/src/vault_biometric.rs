// v1.5.64 — Faz 2.3: Windows Hello biometric unlock for the vault.
//
// Two-layer model so the bio_blob is useless to anyone but THIS user
// on THIS machine:
//
//   1. DPAPI (`CryptProtectData`) wraps the KEK with a key the OS
//      derives from the user's Windows password + machine ID. A
//      raw read of vault.bio_blob from disk gets you nothing — even
//      logged in as a different user on the same box.
//
//   2. UserConsentVerifier (`Windows.Security.Credentials.UI`) gates
//      the unwrap behind a Hello prompt (fingerprint, face, or PIN).
//      The DPAPI wrap is technically transparent to the user, but the
//      Hello prompt is what the user thinks of as "the lock."
//
// Net result: an attacker needs (a) physical access while you're logged
// into Windows AND (b) your fingerprint/PIN to extract the KEK. The
// vault PIN remains the canonical fallback — biometric is purely a
// convenience layer.
//
// macOS/Linux: this whole module compiles to no-op stubs. The vault
// keeps working with the regular PIN unlock there.

#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(not(target_os = "windows"))]
pub use stub_impl::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerifier, UserConsentVerifierAvailability, UserConsentVerificationResult,
    };
    use windows::Win32::Foundation::LocalFree;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    /// True if Windows Hello is set up on this machine (any of:
    /// fingerprint, face, PIN). Returned false on machines where the
    /// API works but no auth method is enrolled, so the FE knows
    /// whether to even surface the "Use Windows Hello" toggle.
    pub fn is_available() -> bool {
        match UserConsentVerifier::CheckAvailabilityAsync() {
            Ok(op) => match op.get() {
                Ok(a) => a == UserConsentVerifierAvailability::Available,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    /// Show the Windows Hello consent prompt. Blocks on the WinRT
    /// `IAsyncOperation` until the user verifies, cancels, or the
    /// device gives up. Returns true only on `Verified`.
    pub fn request_consent(message: &str) -> Result<bool, String> {
        let msg = HSTRING::from(message);
        let op = UserConsentVerifier::RequestVerificationAsync(&msg)
            .map_err(|e| format!("UserConsentVerifier: {}", e))?;
        let res = op
            .get()
            .map_err(|e| format!("verification await: {}", e))?;
        Ok(res == UserConsentVerificationResult::Verified)
    }

    /// DPAPI-wrap a buffer with the *current user* scope. The wrapped
    /// bytes are useless without that user's logon session.
    pub fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let mut data_in = CRYPT_INTEGER_BLOB {
                cbData: plain.len() as u32,
                pbData: plain.as_ptr() as *mut _,
            };
            let mut data_out = CRYPT_INTEGER_BLOB::default();
            CryptProtectData(
                &mut data_in,
                None,                 // no description
                None,                 // optional entropy (we skip — DPAPI default scope is enough)
                None,                 // reserved
                None,                 // no UI prompt struct
                0,                    // flags = current-user scope
                &mut data_out,
            )
            .map_err(|e| format!("CryptProtectData: {}", e))?;
            // Copy out before LocalFree so the Vec owns its bytes.
            let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
            let owned = slice.to_vec();
            let _ = LocalFree(windows::Win32::Foundation::HLOCAL(data_out.pbData as _));
            Ok(owned)
        }
    }

    /// Inverse of `dpapi_protect`. Fails if the blob was created under
    /// a different user account or on a different machine.
    pub fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        unsafe {
            let mut data_in = CRYPT_INTEGER_BLOB {
                cbData: blob.len() as u32,
                pbData: blob.as_ptr() as *mut _,
            };
            let mut data_out = CRYPT_INTEGER_BLOB::default();
            CryptUnprotectData(
                &mut data_in,
                None,
                None,
                None,
                None,
                0,
                &mut data_out,
            )
            .map_err(|e| format!("CryptUnprotectData: {}", e))?;
            let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
            let owned = slice.to_vec();
            let _ = LocalFree(windows::Win32::Foundation::HLOCAL(data_out.pbData as _));
            Ok(owned)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod stub_impl {
    pub fn is_available() -> bool { false }
    pub fn request_consent(_message: &str) -> Result<bool, String> {
        Err("Windows Hello is only available on Windows".into())
    }
    pub fn dpapi_protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI is only available on Windows".into())
    }
    pub fn dpapi_unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI is only available on Windows".into())
    }
}
