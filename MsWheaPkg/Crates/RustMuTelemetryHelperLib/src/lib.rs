//! Rust MU Telemetry Helper
//!
//! Rust helper library for logging telemetry.
//!
//! ## Examples and Usage
//!
//! ```no_run
//! use rust_advanced_logger_dxe::{init_debug, debugln, DEBUG_INFO};
//! use r_efi::efi::Status;
//! pub extern "efiapi" fn efi_main(
//!    _image_handle: *const core::ffi::c_void,
//!    _system_table: *const r_efi::system::SystemTable,
//!  ) -> u64 {
//!
//!    //Initialize debug logging - no output without this.
//!    init_debug(unsafe { (*_system_table).boot_services});
//!
//!    debugln!(DEBUG_INFO, "Hello, World. This is {:} in {:}.", "rust", "UEFI");
//!
//!    Status::SUCCESS.as_usize() as u64
//! }
//! ```
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
#![cfg_attr(target_os = "uefi", no_std)]

mod status_code_runtime;

use mu_pi::protocols::status_code::{EfiStatusCodeType, EfiStatusCodeValue};
use mu_pi::status_code::{EFI_ERROR_CODE, EFI_ERROR_MAJOR, EFI_ERROR_MINOR};
use mu_rust_helpers::{
    boot_services::StandardBootServices,
    guid,
    guid::{guid, guid_fmt},
};
use r_efi::efi;
use rust_advanced_logger_dxe::{debugln, DEBUG_INFO};
use status_code_runtime::{ReportStatusCode, StatusCodeRuntimeProtocol};
use uuid::uuid;

static BOOT_SERVICES: StandardBootServices = StandardBootServices::new_uninit();

const MS_WHEA_RSC_DATA_TYPE_GUID: efi::Guid = guid!("91DEEA05-8C0A-4DCD-B91E-F21CA0C68405");

const MS_WHEA_ERROR_STATUS_TYPE_INFO: EfiStatusCodeType = EFI_ERROR_MINOR | EFI_ERROR_CODE;
const MS_WHEA_ERROR_STATUS_TYPE_FATAL: EfiStatusCodeType = EFI_ERROR_MAJOR | EFI_ERROR_CODE;

/**
 Internal RSC Extended Data Buffer format used by Project Mu firmware WHEA infrastructure.

 A Buffer of this format should be passed to ReportStatusCodeWithExtendedData

 LibraryID:         GUID of the library reporting the error. If not from a library use zero guid
 IhvSharingGuid:    GUID of the partner to share this with. If none use zero guid
 AdditionalInfo1:   64 bit value used for caller to include necessary interrogative information
 AdditionalInfo2:   64 bit value used for caller to include necessary interrogative information
**/
// #pragma pack(1)
// typedef struct {
//     EFI_GUID    LibraryID;
//     EFI_GUID    IhvSharingGuid;
//     UINT64      AdditionalInfo1;
//     UINT64      AdditionalInfo2;
//   } MS_WHEA_RSC_INTERNAL_ERROR_DATA;
// #pragma pack()

#[repr(C)]
struct MsWheaRscInternalErrorData {
    library_id: efi::Guid,
    ihv_sharing_guid: efi::Guid,
    additional_info_1: u64,
    additional_info_2: u64,
}

/// Log telemetry
///
///   @param[in]  ClassId       An EFI_STATUS_CODE_VALUE representing the event that has occurred. This
///                             value will occupy the same space as EventId from LogCriticalEvent(), and
///                             should be unique enough to identify a module or region of code.
///   @param[in]  ExtraData1    [Optional] This should be data specific to the cause. Ideally, used to contain contextual
///                             or runtime data related to the event (e.g. register contents, failure codes, etc.).
///                             It will be persisted.
///   @param[in]  ExtraData2    [Optional] Another UINT64 similar to ExtraData1.
///   @param[in]  ComponentId   [Optional] This identifier should uniquely identify the module that is emitting this
///                             event. When this is passed in as NULL, report status code will automatically populate
///                             this field with gEfiCallerIdGuid.
///   @param[in]  LibraryId     This should identify the library that is emitting this event.
///   @param[in]  IhvId         This should identify the Ihv related to this event if applicable. For example,
///                             this would typically be used for TPM and SOC specific events.
pub fn log_telemetry(
    is_fatal: bool,
    class_id: EfiStatusCodeValue,
    extra_data1: u64,
    extra_data2: u64,
    component_id: Option<&efi::Guid>,
    library_id: Option<&efi::Guid>,
    ihv_id: Option<&efi::Guid>,
) -> Result<(), efi::Status> {
    let status_code_type = if is_fatal { MS_WHEA_ERROR_STATUS_TYPE_FATAL } else { MS_WHEA_ERROR_STATUS_TYPE_INFO };

    let caller_id = component_id.or(Some(&guid::CALLER_ID));

    let error_data = MsWheaRscInternalErrorData {
        library_id: *library_id.unwrap_or(&guid::ZERO),
        ihv_sharing_guid: *ihv_id.unwrap_or(&guid::ZERO),
        additional_info_1: extra_data1,
        additional_info_2: extra_data2,
    };

    debugln!(DEBUG_INFO, "[RustMuTelemetryHelperLib] caller_id: {}", guid_fmt!(caller_id.unwrap()));
    debugln!(DEBUG_INFO, "[RustMuTelemetryHelperLib] extended_data_guid: {}", guid_fmt!(MS_WHEA_RSC_DATA_TYPE_GUID));

    StatusCodeRuntimeProtocol::report_status_code(
        &BOOT_SERVICES,
        status_code_type,
        class_id,
        0,
        caller_id,
        MS_WHEA_RSC_DATA_TYPE_GUID,
        error_data,
    )
}

pub fn init_telemetry(efi_boot_services: &efi::BootServices) {
    BOOT_SERVICES.initialize(efi_boot_services)
}

#[cfg(test)]
mod test {
    use crate::{log_telemetry, MsWheaRscInternalErrorData};
    use core::mem::size_of;

    #[test]
    fn try_log_telemetry() {
        assert_eq!(size_of::<MsWheaRscInternalErrorData>(), 48);
        assert_eq!(Ok(()), log_telemetry(false, 0, 0, 0, None, None, None));
    }
}
