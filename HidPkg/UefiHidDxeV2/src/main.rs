//! HID input driver for UEFI
//!
//! This crate provides input handlers for HID 1.1 compliant keyboards and pointers.
//!
//! ## License
//!
//! Copyright (c) Microsoft Corporation. All rights reserved.
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!

#![cfg_attr(target_os = "uefi", no_std)]
#![cfg_attr(target_os = "uefi", no_main)]
#![allow(non_snake_case)]

#[cfg(target_os = "uefi")]
mod uefi_entry {
    extern crate alloc;
    use alloc::{boxed::Box, vec::Vec};
    use core::{panic::PanicInfo, sync::atomic::Ordering};

    use r_efi::{efi, system};

    use mu_rust_helpers::guid::guid;
    use rust_advanced_logger_dxe::{debugln, init_debug, DEBUG_ERROR};
    use rust_boot_services_allocator_dxe::GLOBAL_ALLOCATOR;
    use rust_mu_telemetry_helper_lib::{init_telemetry, log_telemetry};
    use uefi_hid_dxe_v2::{
        boot_services::UefiBootServices,
        driver_binding::UefiDriverBinding,
        hid::{HidFactory, HidReceiverFactory},
        hid_io::{HidReportReceiver, UefiHidIoFactory},
        keyboard::KeyboardHidHandler,
        pointer::PointerHidHandler,
        BOOT_SERVICES, RUNTIME_SERVICES,
    };
    use uuid::uuid;

    struct UefiReceivers {
        boot_services: &'static dyn UefiBootServices,
        agent: efi::Handle,
    }
    impl HidReceiverFactory for UefiReceivers {
        fn new_hid_receiver_list(
            &self,
            _controller: efi::Handle,
        ) -> Result<Vec<Box<dyn HidReportReceiver>>, efi::Status> {
            let mut receivers: Vec<Box<dyn HidReportReceiver>> = Vec::new();
            receivers.push(Box::new(PointerHidHandler::new(self.boot_services, self.agent)));
            receivers.push(Box::new(KeyboardHidHandler::new(self.boot_services, self.agent)));
            Ok(receivers)
        }
    }

    #[no_mangle]
    pub extern "efiapi" fn efi_main(
        image_handle: efi::Handle,
        system_table: *const system::SystemTable,
    ) -> efi::Status {
        // Safety: This block is unsafe because it assumes that system_table and (*system_table).boot_services are correct,
        // and because it mutates/accesses the global BOOT_SERVICES static.
        unsafe {
            BOOT_SERVICES.initialize((*system_table).boot_services);
            RUNTIME_SERVICES.store((*system_table).runtime_services, Ordering::SeqCst);
            GLOBAL_ALLOCATOR.init((*system_table).boot_services);
            init_debug((*system_table).boot_services);
            init_telemetry((*system_table).boot_services.as_ref().unwrap());
        }

        let hid_io_factory = Box::new(UefiHidIoFactory::new(&BOOT_SERVICES, image_handle));
        let receiver_factory = Box::new(UefiReceivers { boot_services: &BOOT_SERVICES, agent: image_handle });
        let hid_factory = Box::new(HidFactory::new(hid_io_factory, receiver_factory, image_handle));

        let hid_binding = UefiDriverBinding::new(&BOOT_SERVICES, hid_factory, image_handle);
        hid_binding.install().expect("failed to install HID driver binding");

        let _ = log_telemetry(false, 0xA1A2A3A4, 0xB1B2B3B4B5B6B7B8, 0xC1C2C3C4C5C6C7C8, None, None, None);
        let _ = log_telemetry(
            true,
            0xD1D2D3D4,
            0xE1E2E3E4E5E6E7E8,
            0xF1F2F3F4F5F6F7F8,
            None,
            Some(&guid!("8116C328-3DDD-4BB5-A7B0-2098E5E32979")),
            Some(&guid!("347DFE49-9998-4BB1-A152-536FBEDAB6AD")),
        );

        efi::Status::SUCCESS
    }

    #[panic_handler]
    fn panic(info: &PanicInfo) -> ! {
        debugln!(DEBUG_ERROR, "Panic: {:?}", info);
        loop {}
    }
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    //do nothing.
}

#[cfg(test)]
mod test {
    use crate::main;

    #[test]
    fn main_should_do_nothing() {
        main();
    }
}
