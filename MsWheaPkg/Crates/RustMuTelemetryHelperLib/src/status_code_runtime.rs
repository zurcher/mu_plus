use core::{mem, ops::Deref, ptr};

use r_efi::efi;

use boot_services::{allocation::MemoryType, protocol_handler::Protocol, BootServices};
use mu_pi::protocols::status_code;
use mu_pi::protocols::status_code::{EfiStatusCodeData, EfiStatusCodeType, EfiStatusCodeValue};

/// EFI "C" interface for Report Status Code
type EfiReportStatusCode = extern "efiapi" fn(
    r#type: EfiStatusCodeType,
    value: EfiStatusCodeValue,
    instance: u32,
    caller_id: *const efi::Guid,    // Optional
    data: *const EfiStatusCodeData, // Optional
) -> efi::Status;

#[repr(C)]
pub struct StatusCodeRuntimeInterface {
    pub report_status_code: EfiReportStatusCode,
}

pub struct StatusCodeRuntimeProtocol;

impl Deref for StatusCodeRuntimeProtocol {
    type Target = efi::Guid;

    fn deref(&self) -> &Self::Target {
        self.protocol_guid()
    }
}

unsafe impl Protocol for StatusCodeRuntimeProtocol {
    type Interface = StatusCodeRuntimeInterface;

    fn protocol_guid(&self) -> &'static efi::Guid {
        &status_code::PROTOCOL_GUID
    }
}

/// Rust interface for Report Status Code
pub trait ReportStatusCode {
    fn report_status_code<T, B: BootServices>(
        boot_services: &B,
        status_code_type: EfiStatusCodeType,
        status_code_value: EfiStatusCodeValue,
        instance: u32,
        caller_id: Option<&efi::Guid>,
        data_type: efi::Guid,
        data: T,
    ) -> Result<(), efi::Status>;
}

impl ReportStatusCode for StatusCodeRuntimeProtocol {
    fn report_status_code<T, B: BootServices>(
        boot_services: &B,
        status_code_type: EfiStatusCodeType,
        status_code_value: EfiStatusCodeValue,
        instance: u32,
        caller_id: Option<&efi::Guid>,
        data_type: efi::Guid,
        data: T,
    ) -> Result<(), efi::Status> {
        let protocol = boot_services.locate_protocol(&StatusCodeRuntimeProtocol, None)?;
        if protocol.is_none() {
            return Err(efi::Status::NOT_FOUND);
        }

        let data_ptr = boot_services
            .allocate_pool(MemoryType::BOOT_SERVICES_DATA, mem::size_of::<EfiStatusCodeData>() + mem::size_of::<T>())?
            as *mut EfiStatusCodeData;

        unsafe {
            ptr::write(
                data_ptr,
                EfiStatusCodeData {
                    header_size: mem::size_of::<EfiStatusCodeData>() as u16,
                    size: mem::size_of::<T>() as u16,
                    r#type: data_type,
                },
            );
            ptr::write_unaligned(data_ptr.add(1) as *mut T, data);
        };

        let status = (protocol.unwrap().report_status_code)(
            status_code_type,
            status_code_value,
            instance,
            caller_id.map_or(ptr::null(), |x| x as *const _),
            data_ptr,
        );

        if status.is_error() {
            Err(status)
        } else {
            Ok(())
        }
    }
}
