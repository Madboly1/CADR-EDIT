//! Windows DPAPI (Data Protection API) bindings used by the engine's secret store to keep
//! user-supplied API keys encrypted at rest (`CryptProtectData` / `CryptUnprotectData` in
//! `crypt32.dll`, `CRYPTPROTECT_UI_FORBIDDEN`, current-user scope).
//!
//! FFI module (docs/adr/0001-platform-ffi.md): every `unsafe` block has a `// SAFETY:` comment,
//! the public items are safe and return `Result<_, String>`.

const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

#[repr(C)]
struct DataBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[link(name = "crypt32")]
unsafe extern "system" {
    fn CryptProtectData(
        p_data_in: *const DataBlob,
        sz_data_descr: *const u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;

    fn CryptUnprotectData(
        p_data_in: *const DataBlob,
        ppsz_data_descr: *mut *mut u16,
        p_optional_entropy: *const DataBlob,
        pv_reserved: *mut std::ffi::c_void,
        p_prompt_struct: *mut std::ffi::c_void,
        dw_flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// Encrypt `plaintext` with DPAPI for the current user. The returned blob can only be decrypted
/// by the same user on the same machine.
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut input = plaintext.to_vec();
    let in_blob = DataBlob { cb_data: u32::try_from(input.len()).map_err(|_| "secret too large for DPAPI".to_string())?, pb_data: input.as_mut_ptr() };
    let mut out_blob = DataBlob { cb_data: 0, pb_data: std::ptr::null_mut() };
    // SAFETY: `in_blob` points to `input`, which outlives the call; `out_blob` is written by the
    // API on success; all optional pointers are null and flags forbid UI, so the call is
    // synchronous and cannot re-enter.
    let ok = unsafe {
        CryptProtectData(&in_blob, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), CRYPTPROTECT_UI_FORBIDDEN, &mut out_blob)
    };
    if ok == 0 {
        return Err("Windows DPAPI CryptProtectData failed".into());
    }
    // SAFETY: on success `out_blob.pb_data` points to `out_blob.cb_data` bytes allocated by
    // CryptProtectData, valid until freed below.
    let slice = unsafe { std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize) };
    let result = slice.to_vec();
    // SAFETY: `out_blob.pb_data` came from CryptProtectData and is freed exactly once, after the
    // copy above.
    unsafe {
        LocalFree(out_blob.pb_data.cast());
    }
    Ok(result)
}

/// Decrypt a blob produced by [`protect`]. Only the same user on the same machine can decrypt it.
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    let mut input = ciphertext.to_vec();
    let in_blob = DataBlob { cb_data: u32::try_from(input.len()).map_err(|_| "ciphertext too large for DPAPI".to_string())?, pb_data: input.as_mut_ptr() };
    let mut out_blob = DataBlob { cb_data: 0, pb_data: std::ptr::null_mut() };
    // SAFETY: `in_blob` points to `input`, which outlives the call; `out_blob` is written by the
    // API on success; all optional pointers are null and flags forbid UI, so the call is
    // synchronous and cannot re-enter.
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err("Windows DPAPI CryptUnprotectData failed".into());
    }
    // SAFETY: on success `out_blob.pb_data` points to `out_blob.cb_data` bytes allocated by
    // CryptUnprotectData, valid until freed below.
    let slice = unsafe { std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize) };
    let result = slice.to_vec();
    // SAFETY: `out_blob.pb_data` came from CryptUnprotectData and is freed exactly once, after the
    // copy above.
    unsafe {
        LocalFree(out_blob.pb_data.cast());
    }
    Ok(result)
}
