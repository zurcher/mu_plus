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
    boot_services::{BootServices, StandardBootServices},
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
#[cfg(not(tarpaulin_include))]
pub fn log_telemetry(
    is_fatal: bool,
    class_id: EfiStatusCodeValue,
    extra_data1: u64,
    extra_data2: u64,
    component_id: Option<&efi::Guid>,
    library_id: Option<&efi::Guid>,
    ihv_id: Option<&efi::Guid>,
) -> Result<(), efi::Status> {
    log_telemetry_internal(
        &BOOT_SERVICES,
        is_fatal,
        class_id,
        extra_data1,
        extra_data2,
        component_id,
        library_id,
        ihv_id,
    )
}

fn log_telemetry_internal<B: BootServices>(
    boot_services: &B,
    is_fatal: bool,
    class_id: EfiStatusCodeValue,
    extra_data1: u64,
    extra_data2: u64,
    component_id: Option<&efi::Guid>,
    library_id: Option<&efi::Guid>,
    ihv_id: Option<&efi::Guid>,
) -> Result<(), efi::Status> {
    let status_code_type: EfiStatusCodeType =
        if is_fatal { MS_WHEA_ERROR_STATUS_TYPE_FATAL } else { MS_WHEA_ERROR_STATUS_TYPE_INFO };

    let error_data = MsWheaRscInternalErrorData {
        library_id: *library_id.unwrap_or(&guid::ZERO),
        ihv_sharing_guid: *ihv_id.unwrap_or(&guid::ZERO),
        additional_info_1: extra_data1,
        additional_info_2: extra_data2,
    };

    debugln!(DEBUG_INFO, "[RustMuTelemetryHelperLib] extended_data_guid: {}", guid_fmt!(MS_WHEA_RSC_DATA_TYPE_GUID));

    StatusCodeRuntimeProtocol::report_status_code(
        boot_services,
        status_code_type,
        class_id,
        0,
        component_id,
        MS_WHEA_RSC_DATA_TYPE_GUID,
        error_data,
    )
}

#[cfg(not(tarpaulin_include))]
pub fn init_telemetry(efi_boot_services: &efi::BootServices) {
    BOOT_SERVICES.initialize(efi_boot_services)
}

#[cfg(test)]
#[allow(unused_imports)]
mod test {
    use boot_services::{allocation::MemoryType, BootServices, MockBootServices};
    use mu_pi::protocols::status_code::{EfiStatusCodeData, EfiStatusCodeType, EfiStatusCodeValue};
    use mu_rust_helpers::guid::{guid, guid_fmt};
    use r_efi::efi;
    use rust_advanced_logger_dxe::{debugln, DEBUG_INFO};
    use uuid::uuid;

    use crate::{
        log_telemetry_internal,
        status_code_runtime::{StatusCodeRuntimeInterface, StatusCodeRuntimeProtocol},
        MsWheaRscInternalErrorData, MS_WHEA_ERROR_STATUS_TYPE_FATAL,
    };
    use core::mem::size_of;

    const DATA_SIZE: usize = size_of::<EfiStatusCodeData>() + size_of::<MsWheaRscInternalErrorData>();
    const MOCK_CALLER_ID: efi::Guid = guid!("d0d1d2d3-d4d5-d6d7-d8d9-dadbdcdddedf");
    const MOCK_STATUS_CODE_VALUE: EfiStatusCodeValue = 0xa0a1a2a3;

    extern "efiapi" fn mock_report_status_code(
        r#type: EfiStatusCodeType,
        value: EfiStatusCodeValue,
        instance: u32,
        caller_id: *const efi::Guid,     // Optional
        _data: *const EfiStatusCodeData, // Optional
    ) -> efi::Status {
        assert_eq!(r#type, MS_WHEA_ERROR_STATUS_TYPE_FATAL);
        assert_eq!(value, MOCK_STATUS_CODE_VALUE);
        assert_eq!(instance, 0);
        assert_eq!(unsafe { *caller_id }, MOCK_CALLER_ID);
        debugln!(DEBUG_INFO, "[MockStatusCodeRuntime] caller_id: {}", guid_fmt!(unsafe { *caller_id }));
        efi::Status::SUCCESS
    }

    static MOCK_STATUS_CODE_RUNTIME_INTERFACE: StatusCodeRuntimeInterface =
        StatusCodeRuntimeInterface { report_status_code: mock_report_status_code };

    #[test]
    fn try_log_telemetry() {
        let mut mock_boot_services: MockBootServices = MockBootServices::new();

        mock_boot_services.expect_locate_protocol().returning(|_: &StatusCodeRuntimeProtocol, registration| unsafe {
            assert_eq!(registration, None);
            Ok(Some(
                (&MOCK_STATUS_CODE_RUNTIME_INTERFACE as *const StatusCodeRuntimeInterface
                    as *mut StatusCodeRuntimeInterface)
                    .as_mut()
                    .unwrap(),
            ))
        });

        assert_eq!(size_of::<MsWheaRscInternalErrorData>(), 48);
        assert_eq!(size_of::<[u8; 68]>(), DATA_SIZE);
        assert_eq!(
            Ok(()),
            log_telemetry_internal(
                &mock_boot_services,
                true,
                MOCK_STATUS_CODE_VALUE,
                0xb0b1b2b3b4b5b6b7,
                0xc0c1c2c3c4c5c6c7,
                Some(&MOCK_CALLER_ID),
                Some(&guid!("e0e1e2e3-e4e5-e6e7-e8e9-eaebecedeeef")),
                Some(&guid!("f0f1f2f3-f4f5-f6f7-f8f9-fafbfcfdfeff"))
            )
        );
    }
}
