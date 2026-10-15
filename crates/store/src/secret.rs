//! Sealing voiceprints at rest. On Windows they are encrypted with DPAPI for the
//! signed-in user, so another account (or a copied disk) cannot read them; elsewhere
//! they are stored as-is (Linux and macOS builds are for development until a keyring
//! backend is added).

/// First byte of a sealed blob: how the rest is stored.
const PLAIN: u8 = b'P';
#[cfg_attr(not(windows), allow(dead_code))]
const DPAPI: u8 = b'D';

/// Seal `data` for this user.
pub fn seal(data: &[u8]) -> Vec<u8> {
    #[cfg(windows)]
    match dpapi::protect(data) {
        Ok(sealed) => return [&[DPAPI], sealed.as_slice()].concat(),
        Err(e) => tracing::warn!("DPAPI unavailable, storing the voiceprint unsealed: {e}"),
    }
    [&[PLAIN], data].concat()
}

/// Open a blob from [`seal`]; `None` when it cannot be read here (another user's or
/// another machine's DPAPI blob, or corrupt).
pub fn open(blob: &[u8]) -> Option<Vec<u8>> {
    match blob.split_first()? {
        (&PLAIN, rest) => Some(rest.to_vec()),
        #[cfg(windows)]
        (&DPAPI, rest) => dpapi::unprotect(rest)
            .map_err(|e| tracing::warn!("could not unseal a voiceprint: {e}"))
            .ok(),
        _ => None,
    }
}

/// `f32`s as little-endian bytes.
pub fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Little-endian bytes as `f32`s; a ragged tail is dropped.
pub fn bytes_to_floats(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod dpapi {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr().cast_mut(),
        }
    }

    /// Copy DPAPI's output and free it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with LocalAlloc;
        // they are copied out before being freed exactly once.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
            v
        }
    }

    pub fn protect(data: &[u8]) -> windows::core::Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `data`, alive for the call; `out` is written by DPAPI.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out))
    }

    pub fn unprotect(data: &[u8]) -> windows::core::Result<Vec<u8>> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
        }
        Ok(take(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let v = vec![0.25f32, -1.5, 3.0];
        let sealed = seal(&floats_to_bytes(&v));
        assert_eq!(bytes_to_floats(&open(&sealed).unwrap()), v);
        assert!(open(&[]).is_none());
        assert!(open(b"Xjunk").is_none());
        if cfg!(windows) {
            assert_eq!(sealed[0], DPAPI);
        }
    }
}
