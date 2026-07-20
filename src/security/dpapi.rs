//! Small, platform-aware wrapper around Windows Data Protection API.

use anyhow::Result;

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result, bail};
    use std::slice;
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
            },
        },
        core::PCWSTR,
    };

    struct LocalBuffer(*mut u8);

    impl Drop for LocalBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0.cast())));
                }
            }
        }
    }

    fn input_blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB> {
        let len = u32::try_from(data.len()).context("data is too large for DPAPI")?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: len,
            // The API does not mutate the input despite using a mutable pointer in DATA_BLOB.
            pbData: data.as_ptr().cast_mut(),
        })
    }

    unsafe fn copy_output(blob: &CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
        if blob.cbData != 0 && blob.pbData.is_null() {
            bail!("DPAPI returned an invalid output buffer");
        }
        let guard = LocalBuffer(blob.pbData);
        let result = if blob.cbData == 0 {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(guard.0, blob.cbData as usize) }.to_vec()
        };
        Ok(result)
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let input = input_blob(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("CryptProtectData failed")?;
            copy_output(&output)
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let input = input_blob(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("CryptUnprotectData failed")?;
            copy_output(&output)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::Result;

    // DPAPI does not exist off Windows. Keeping this pass-through fallback makes
    // development builds portable; callers should treat it as unencrypted.
    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

/// Protect bytes for the current Windows user.
pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
    imp::protect(data)
}

/// Recover bytes previously returned by [`protect`].
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    imp::unprotect(data)
}
