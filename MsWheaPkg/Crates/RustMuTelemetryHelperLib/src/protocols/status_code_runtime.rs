use mu_pi::protocols::status_code::{EfiStatusCodeData, EfiStatusCodeType, EfiStatusCodeValue};
use mu_rust_helpers::guid::guid;
use r_efi::efi;
use uuid::uuid;

pub const PROTOCOL_GUID: efi::Guid = guid!("D2B2B828-0826-48A7-B3DF-983C006024F0");

pub type EfiReportStatusCode = extern "efiapi" fn(
    r#type: EfiStatusCodeType,
    value: EfiStatusCodeValue,
    instance: u32,
    caller_id: *const efi::Guid,    // Optional
    data: *const EfiStatusCodeData, // Optional
) -> efi::Status;

pub struct Protocol {
    pub report_status_code: EfiReportStatusCode,
}
