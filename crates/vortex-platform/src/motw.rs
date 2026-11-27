//! Windows Mark-of-the-Web (`:Zone.Identifier`) and `IAttachmentExecute` AV integration (`TH-07`).

use std::path::Path;

use vortex_application::AppResult;
use vortex_error::TraceId;
use vortex_observability::RedactedUrl;

fn build_zone_identifier_content(source_url: &str, referrer_url: Option<&str>) -> String {
    let clean_host = RedactedUrl::parse(source_url).to_string();
    let mut content = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(ref_raw) = referrer_url.filter(|s| !s.trim().is_empty()) {
        let clean_ref = RedactedUrl::parse(ref_raw).to_string();
        content.push_str("ReferrerUrl=");
        content.push_str(&clean_ref);
        content.push_str("\r\n");
    }
    content.push_str("HostUrl=");
    content.push_str(&clean_host);
    content.push_str("\r\n");
    content
}

/// Applies `:Zone.Identifier` and invokes COM `IAttachmentExecute` (`CLSID_AttachmentServices`)
/// on a dedicated blocking STA thread (`TH-07`).
///
/// # Errors
/// Returns `VTX-SEC-001` if Windows Defender / Attachment Services blocks the file as malware.
pub async fn apply_motw_and_av(
    final_path: &Path,
    source_url: &str,
    referrer_url: Option<&str>,
    trace_id: TraceId,
) -> AppResult<()> {
    let path_buf = final_path.to_path_buf();
    let zone_text = build_zone_identifier_content(source_url, referrer_url);
    let clean_source = RedactedUrl::parse(source_url).to_string();
    let clean_ref = referrer_url.map(|r| RedactedUrl::parse(r).to_string());

    tokio::task::spawn_blocking(move || {
        let ads_path_str = format!("{}:Zone.Identifier", path_buf.display());
        if let Err(err) = std::fs::write(&ads_path_str, zone_text.as_bytes()) {
            tracing::warn!(
                %trace_id,
                detail = %err,
                "Could not write :Zone.Identifier ADS (filesystem may be exFAT/FAT32)"
            );
        }
        invoke_attachment_execute_com(&path_buf, &clean_source, clean_ref.as_deref(), trace_id)
    })
    .await
    .map_err(|e| {
        vortex_error::VortexError::coded(
            vortex_error::ErrorCode::BugInternalIsolated,
            trace_id,
            format!("MotW/AV blocking task panicked: {e}"),
        )
    })?
}

#[allow(unsafe_code, clippy::cast_sign_loss)]
fn invoke_attachment_execute_com(
    final_path: &Path,
    clean_source: &str,
    clean_referrer: Option<&str>,
    trace_id: TraceId,
) -> AppResult<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows_sys::core::{GUID, HRESULT};

    static CLSID_ATTACHMENT_SERVICES: GUID = GUID {
        data1: 0x4125_DD96,
        data2: 0xE03A,
        data3: 0x4103,
        data4: [0x8F, 0x70, 0xE0, 0x59, 0x7D, 0x80, 0x3B, 0x9C],
    };
    static IID_IATTACHMENT_EXECUTE: GUID = GUID {
        data1: 0x73DB_1241,
        data2: 0x1E85,
        data3: 0x4581,
        data4: [0x8E, 0x4F, 0xA8, 0x1E, 0x1D, 0x0F, 0x8C, 0x57],
    };
    static VORTEX_CLIENT_GUID: GUID = GUID {
        data1: 0x9F72_8C41,
        data2: 0x3B92,
        data3: 0x4D1E,
        data4: [0xA4, 0x11, 0x52, 0x80, 0x01, 0x26, 0x11, 0x01],
    };
    const HR_VIRUS_INFECTED: HRESULT = -2_147_024_671;
    const HR_VIRUS_DELETED: HRESULT = -2_147_024_670;
    const HR_SECURITY_PROBLEM: HRESULT = -2_146_697_202;

    #[repr(C)]
    struct IAttachmentExecuteVtbl {
        query_interface: extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: extern "system" fn(*mut c_void) -> u32,
        release: extern "system" fn(*mut c_void) -> u32,
        set_client_title: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_client_guid: extern "system" fn(*mut c_void, *const GUID) -> HRESULT,
        set_local_path: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_file_name: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_source: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        set_referrer: extern "system" fn(*mut c_void, *const u16) -> HRESULT,
        check_policy: extern "system" fn(*mut c_void) -> HRESULT,
        prompt: extern "system" fn(*mut c_void, *mut c_void, i32, *mut i32) -> HRESULT,
        save: extern "system" fn(*mut c_void) -> HRESULT,
        execute:
            extern "system" fn(*mut c_void, *mut c_void, *const u16, *mut *mut c_void) -> HRESULT,
        save_with_ui: extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        clear_client_state: extern "system" fn(*mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IAttachmentExecuteRaw {
        lp_vtbl: *const IAttachmentExecuteVtbl,
    }

    fn to_wide_null(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    // SAFETY: Initializes STA COM on this dedicated `spawn_blocking` worker thread and balances
    // successful `hr >= 0` initialization with `CoUninitialize` before returning.
    unsafe {
        let init_hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let should_uninit = init_hr >= 0;

        let mut instance: *mut c_void = std::ptr::null_mut();
        let create_hr = CoCreateInstance(
            &raw const CLSID_ATTACHMENT_SERVICES,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &raw const IID_IATTACHMENT_EXECUTE,
            &raw mut instance,
        );

        let mut scan_result = Ok(());
        if create_hr >= 0 && !instance.is_null() {
            let obj = instance.cast::<IAttachmentExecuteRaw>();
            let vtbl = &*(*obj).lp_vtbl;

            let title_w = to_wide_null(std::ffi::OsStr::new("Vortex Download Manager"));
            let path_w = to_wide_null(final_path.as_os_str());
            let src_w = to_wide_null(std::ffi::OsStr::new(clean_source));

            let _ = (vtbl.set_client_title)(instance, title_w.as_ptr());
            let _ = (vtbl.set_client_guid)(instance, &raw const VORTEX_CLIENT_GUID);
            let _ = (vtbl.set_local_path)(instance, path_w.as_ptr());
            let _ = (vtbl.set_source)(instance, src_w.as_ptr());
            if let Some(refer) = clean_referrer {
                let ref_w = to_wide_null(std::ffi::OsStr::new(refer));
                let _ = (vtbl.set_referrer)(instance, ref_w.as_ptr());
            }

            let save_hr = (vtbl.save)(instance);
            if save_hr == HR_VIRUS_INFECTED
                || save_hr == HR_VIRUS_DELETED
                || save_hr == HR_SECURITY_PROBLEM
            {
                scan_result = Err(vortex_error::VortexError::coded(
                    vortex_error::ErrorCode::SecAntivirusQuarantined,
                    trace_id,
                    format!(
                        "Windows Defender / Attachment Services quarantined or blocked file (HRESULT {save_hr:#X})"
                    ),
                ));
            }
            let _ = (vtbl.release)(instance);
        }

        if should_uninit {
            CoUninitialize();
        }
        scan_result
    }
}
