//! Simple Text In Protocol FFI Support.
//!
//! This module manages the Simple Text In FFI.
//!
//! ## License
//!
//! Copyright (c) Microsoft Corporation. All rights reserved.
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!

use alloc::boxed::Box;
use core::{ffi::c_void, ptr};

use r_efi::{efi, protocols};
use rust_advanced_logger_dxe::{debugln, DEBUG_ERROR};

use boot_services::{event::EventType, tpl::Tpl, BootServices};

use crate::{
    hid_io::{HidIoFactory, UefiHidIoFactory},
    keyboard::KeyboardHidHandler,
    static_boot_services,
};

/// FFI context
/// # Safety
/// A pointer to KeyboardHidHandler is included in the contexts so that it can be reclaimed in the simple_text_in API
/// implementations. Care must be taken to ensure that rust invariants are respected when accessing the
/// KeyboardHidHandler. In particular, the design must ensure mutual exclusion on the KeyboardHidHandler between
/// callbacks running at different TPL; this is accomplished by ensuring all access to the structure is at TPL_NOTIFY
/// once initialization is complete - for this reason the context structure includes a direct reference to boot_services
/// so that TPL can be enforced without access to the *mut KeyboardHidHandler.
///
/// In addition, the simple_text_in protocol element needs to be the first element in the structure so that the full
/// structure can be recovered by simple casting for simple_text_in_ex FFI interfaces that only receive a pointer to the
/// simple_text_in protocol structure.
#[repr(C)]
pub struct SimpleTextInFfi {
    simple_text_in: protocols::simple_text_input::Protocol,
    keyboard_handler: *mut KeyboardHidHandler,
}

impl SimpleTextInFfi {
    /// Installs the simple text in protocol
    pub fn install(controller: efi::Handle, keyboard_handler: &mut KeyboardHidHandler) -> Result<(), efi::Status> {
        //Create simple_text_in context
        let simple_text_in_ctx = SimpleTextInFfi {
            simple_text_in: protocols::simple_text_input::Protocol {
                reset: Self::simple_text_in_reset,
                read_key_stroke: Self::simple_text_in_read_key_stroke,
                wait_for_key: ptr::null_mut(),
            },
            keyboard_handler: keyboard_handler as *mut KeyboardHidHandler,
        };

        let simple_text_in_ptr = Box::into_raw(Box::new(simple_text_in_ctx));

        //create event for wait_for_key
        let status = unsafe {
            static_boot_services().create_event_unchecked(
                EventType::NOTIFY_WAIT,
                Tpl::NOTIFY,
                Some(Self::simple_text_in_wait_for_key),
                simple_text_in_ptr as *mut c_void,
            )
        };
        if status.is_err() {
            drop(unsafe { Box::from_raw(simple_text_in_ptr) });
            status?;
        }
        let wait_for_key_event: efi::Event = status.unwrap();

        unsafe { (*simple_text_in_ptr).simple_text_in.wait_for_key = wait_for_key_event };

        //install the simple_text_in protocol
        let status = unsafe {
            static_boot_services().install_protocol_interface_unchecked(
                Some(controller),
                &r_efi::protocols::simple_text_input::PROTOCOL_GUID,
                simple_text_in_ptr as *mut c_void,
            )
        };

        if status.is_err() {
            let _ = static_boot_services().close_event(wait_for_key_event);
            drop(unsafe { Box::from_raw(simple_text_in_ptr) });
            status?;
        }

        Ok(())
    }

    /// Uninstalls the simple text in protocol
    pub fn uninstall(agent: efi::Handle, controller: efi::Handle) -> Result<(), efi::Status> {
        //Controller is set - that means initialize() was called, and there is potential state exposed thru FFI that needs
        //to be cleaned up.
        let status = unsafe {
            static_boot_services().open_protocol_unchecked(
                controller,
                &r_efi::protocols::simple_text_input::PROTOCOL_GUID,
                agent,
                controller,
                efi::OPEN_PROTOCOL_GET_PROTOCOL,
            )
        };
        if status.is_err() {
            //No protocol is actually installed on this controller, so nothing to clean up.
            return Ok(());
        }
        let simple_text_in_ptr: *mut SimpleTextInFfi = status.unwrap() as *mut SimpleTextInFfi;

        //Attempt to uninstall the simple_text_in interface - this should disconnect any drivers using it and release
        //the interface.
        let status = unsafe {
            static_boot_services().uninstall_protocol_interface_unchecked(
                controller,
                &r_efi::protocols::simple_text_input::PROTOCOL_GUID,
                simple_text_in_ptr as *mut c_void,
            )
        };
        if status.is_err() {
            //An error here means some other driver might be holding on to the simple_text_in_ptr.
            //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on absolute_pointer_ptr to continue to resolve
            //and return error based on observing keyboard_handler is null.
            debugln!(DEBUG_ERROR, "Failed to uninstall simple_text_in interface, status: {:x?}", status);

            unsafe {
                (*simple_text_in_ptr).keyboard_handler = ptr::null_mut();
            }
            //return without tearing down the context.
            status?;
        }

        let wait_for_key_event: efi::Handle = unsafe { (*simple_text_in_ptr).simple_text_in.wait_for_key };
        let status = static_boot_services().close_event(wait_for_key_event);
        if status.is_err() {
            //An error here means the event was not closed, so in theory the notification_callback on it could still be
            //fired.
            //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on simple_text_in_ptr to continue to resolve
            //and return error based on observing keyboard_handler is null.
            debugln!(DEBUG_ERROR, "Failed to close simple_text_in_ptr.wait_for_key event, status: {:x?}", status);
            unsafe {
                (*simple_text_in_ptr).keyboard_handler = ptr::null_mut();
            }
            status?;
        }
        // None of the parts of simple_text_in_ptr are in use, so it is safe to drop it.
        drop(unsafe { Box::from_raw(simple_text_in_ptr) });
        Ok(())
    }

    // resets the keyboard state - part of the simple_text_in protocol interface.
    extern "efiapi" fn simple_text_in_reset(
        this: *mut protocols::simple_text_input::Protocol,
        extended_verification: efi::Boolean,
    ) -> efi::Status {
        if this.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let mut status = efi::Status::DEVICE_ERROR;
        '_reset_processing: {
            let keyboard_handler = unsafe { context.keyboard_handler.as_mut() };
            if let Some(keyboard_handler) = keyboard_handler {
                //reset requires an instance of hid_io to allow for LED updating, so use UefiHidIoFactory to build one.
                if let Some(controller) = keyboard_handler.controller() {
                    let hid_io = UefiHidIoFactory::new(keyboard_handler.agent()).new_hid_io(controller, false);
                    if let Ok(hid_io) = hid_io {
                        if let Err(err) = keyboard_handler.reset(hid_io.as_ref(), extended_verification.into()) {
                            status = err;
                        } else {
                            status = efi::Status::SUCCESS;
                        }
                    }
                }
            }
        }
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // reads a key stroke - part of the simple_text_in protocol interface.
    extern "efiapi" fn simple_text_in_read_key_stroke(
        this: *mut protocols::simple_text_input::Protocol,
        key: *mut protocols::simple_text_input::InputKey,
    ) -> efi::Status {
        if this.is_null() || key.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInFfi).as_mut() }.expect("bad pointer");
        let status;
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        'read_key_stroke: {
            let keyboard_handler = unsafe { context.keyboard_handler.as_mut() };
            if let Some(keyboard_handler) = keyboard_handler {
                loop {
                    if let Some(mut key_data) = keyboard_handler.pop_key() {
                        // skip partials
                        if key_data.key.unicode_char == 0 && key_data.key.scan_code == 0 {
                            continue;
                        }
                        //translate ctrl-alpha to corresponding control value. ctrl-a = 0x0001, ctrl-z = 0x001A
                        const CONTROL_PRESSED: u32 = protocols::simple_text_input_ex::RIGHT_CONTROL_PRESSED
                            | protocols::simple_text_input_ex::LEFT_CONTROL_PRESSED;
                        if (key_data.key_state.key_shift_state & CONTROL_PRESSED) != 0 {
                            if key_data.key.unicode_char >= 0x0061 && key_data.key.unicode_char <= 0x007a {
                                //'a' to 'z'
                                key_data.key.unicode_char = (key_data.key.unicode_char - 0x0061) + 1;
                            }
                            if key_data.key.unicode_char >= 0x0041 && key_data.key.unicode_char <= 0x005a {
                                //'A' to 'Z'
                                key_data.key.unicode_char = (key_data.key.unicode_char - 0x0041) + 1;
                            }
                        }
                        unsafe { key.write(key_data.key) }
                        status = efi::Status::SUCCESS;
                    } else {
                        status = efi::Status::NOT_READY;
                    }
                    break 'read_key_stroke;
                }
            } else {
                status = efi::Status::DEVICE_ERROR;
                break 'read_key_stroke;
            }
        }
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // Event handler function for the wait_for_key_event
    extern "efiapi" fn simple_text_in_wait_for_key(event: efi::Event, context: *mut c_void) {
        if context.is_null() {
            debugln!(DEBUG_ERROR, "simple_text_in_wait_for_key invoked with invalid context");
            return;
        }
        let context = unsafe { (context as *mut SimpleTextInFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        {
            if let Some(keyboard_handler) = unsafe { context.keyboard_handler.as_mut() } {
                while let Some(key_data) = keyboard_handler.peek_key() {
                    if key_data.key.unicode_char == 0 && key_data.key.scan_code == 0 {
                        // consume (and ignore) the partial stroke.
                        let _ = keyboard_handler.pop_key();
                        continue;
                    } else {
                        // valid keystroke
                        let _ = static_boot_services().signal_event(event);
                        break;
                    }
                }
            }
        }
        static_boot_services().restore_tpl(old_tpl);
    }
}
