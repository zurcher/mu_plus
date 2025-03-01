//! Absolute Pointer Protocol FFI Support.
//!
//! This module manages the Absolute Pointer Protocol FFI.
//!
//! ## License
//!
//! Copyright (c) Microsoft Corporation. All rights reserved.
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
use alloc::boxed::Box;
use core::{ffi::c_void, ptr};

use r_efi::{efi, protocols};

use hidparser::report_data_types::Usage;
use rust_advanced_logger_dxe::{debugln, DEBUG_ERROR, DEBUG_INFO, DEBUG_WARN};

use super::{PointerHidHandler, BUTTON_MAX, BUTTON_MIN, DIGITIZER_SWITCH_MAX, DIGITIZER_SWITCH_MIN};
use crate::static_boot_services;
use boot_services::{event::EventType, tpl::Tpl, BootServices};

// FFI context
// Safety: a pointer to PointerHidHandler is included in the context so that it can be reclaimed in the absolute_pointer
// API implementation. Care must be taken to ensure that rust invariants are respected when accessing the
// PointerHidHandler. In particular, the design must ensure mutual exclusion on the PointerHidHandler between callbacks
// running at different TPL; this is accomplished by ensuring all access to the structure is at TPL_NOTIFY once
// initialization is complete - for this reason the context structure includes a direct reference to boot_services so
// that TPL can be enforced without access to the *mut PointerHidHandle.
//
// In addition, the absolute_pointer element needs to be the first element in the structure so that the full structure
// can be recovered by simple casting for absolute_pointer FFI interfaces that only receive a pointer to the
// absolute_pointer structure.
#[repr(C)]
pub struct PointerContext {
    absolute_pointer: protocols::absolute_pointer::Protocol,
    pointer_handler: *mut PointerHidHandler,
}

impl Drop for PointerContext {
    fn drop(&mut self) {
        if !self.absolute_pointer.mode.is_null() {
            drop(unsafe { Box::from_raw(self.absolute_pointer.mode) });
        }
    }
}

impl PointerContext {
    /// Installs the absolute pointer protocol
    pub fn install(controller: efi::Handle, pointer_handler: &mut PointerHidHandler) -> Result<(), efi::Status> {
        // Create pointer context.
        let pointer_ctx = PointerContext {
            absolute_pointer: protocols::absolute_pointer::Protocol {
                reset: Self::absolute_pointer_reset,
                get_state: Self::absolute_pointer_get_state,
                mode: Box::into_raw(Box::new(Self::initialize_mode(pointer_handler))),
                wait_for_input: ptr::null_mut(),
            },
            pointer_handler: pointer_handler as *mut PointerHidHandler,
        };

        let absolute_pointer_ptr = Box::into_raw(Box::new(pointer_ctx));

        // create event for wait_for_input.
        let status = unsafe {
            static_boot_services().create_event_unchecked(
                EventType::NOTIFY_WAIT,
                Tpl::NOTIFY,
                Some(Self::wait_for_pointer),
                absolute_pointer_ptr as *mut c_void,
            )
        };
        if status.is_err() {
            drop(unsafe { Box::from_raw(absolute_pointer_ptr) });
            status?;
        }
        let wait_for_pointer_input_event: efi::Event = status.unwrap();

        unsafe { (*absolute_pointer_ptr).absolute_pointer.wait_for_input = wait_for_pointer_input_event };

        // install the absolute_pointer protocol.
        let status = unsafe {
            static_boot_services().install_protocol_interface_unchecked(
                Some(controller),
                &r_efi::protocols::absolute_pointer::PROTOCOL_GUID,
                absolute_pointer_ptr as *mut c_void,
            )
        };

        if status.is_err() {
            let _ = static_boot_services().close_event(wait_for_pointer_input_event);
            drop(unsafe { Box::from_raw(absolute_pointer_ptr) });
            status?;
        }

        Ok(())
    }

    // Initializes the absolute_pointer mode structure.
    fn initialize_mode(pointer_handler: &PointerHidHandler) -> protocols::absolute_pointer::Mode {
        let mut mode: protocols::absolute_pointer::Mode = Default::default();

        if pointer_handler.supported_usages.contains(&Usage::from(super::GENERIC_DESKTOP_X)) {
            mode.absolute_max_x = super::AXIS_RESOLUTION;
            mode.absolute_min_x = 0;
        } else {
            debugln!(DEBUG_WARN, "No x-axis usages found in the report descriptor.");
        }

        if pointer_handler.supported_usages.contains(&Usage::from(super::GENERIC_DESKTOP_Y)) {
            mode.absolute_max_y = super::AXIS_RESOLUTION;
            mode.absolute_min_y = 0;
        } else {
            debugln!(DEBUG_WARN, "No y-axis usages found in the report descriptor.");
        }

        if (pointer_handler.supported_usages.contains(&Usage::from(super::GENERIC_DESKTOP_Z)))
            || (pointer_handler.supported_usages.contains(&Usage::from(super::GENERIC_DESKTOP_WHEEL)))
        {
            mode.absolute_max_z = super::AXIS_RESOLUTION;
            mode.absolute_min_z = 0;
            //TODO: Z-axis is interpreted as pressure data. This is for compat with reference implementation in C, but
            //could consider e.g. looking for actual digitizer tip pressure usages or something.
            mode.attributes |= 0x02;
        } else {
            debugln!(DEBUG_INFO, "No z-axis usages found in the report descriptor.");
        }

        let button_count = pointer_handler
            .supported_usages
            .iter()
            .filter(|x| matches!((**x).into(), BUTTON_MIN..=BUTTON_MAX | DIGITIZER_SWITCH_MIN..=DIGITIZER_SWITCH_MAX))
            .count();

        if button_count > 1 {
            mode.attributes |= 0x01; // alternate button exists.
        }

        mode
    }

    /// Uninstalls the absolute pointer protocol
    pub fn uninstall(agent: efi::Handle, controller: efi::Handle) -> Result<(), efi::Status> {
        let status = unsafe {
            static_boot_services().open_protocol_unchecked(
                controller,
                &r_efi::protocols::absolute_pointer::PROTOCOL_GUID,
                agent,
                controller,
                efi::OPEN_PROTOCOL_GET_PROTOCOL,
            )
        };
        if status.is_err() {
            //No protocol is actually installed on this controller, so nothing to clean up.
            return Ok(());
        }
        let absolute_pointer_ptr: *mut PointerContext = status.unwrap() as *mut PointerContext;

        //Attempt to uninstall the absolute_pointer interface - this should disconnect any drivers using it and release
        //the interface.
        let status = unsafe {
            static_boot_services().uninstall_protocol_interface_unchecked(
                controller,
                &r_efi::protocols::absolute_pointer::PROTOCOL_GUID,
                absolute_pointer_ptr as *mut c_void,
            )
        };
        if status.is_err() {
            //An error here means some other driver might be holding on to the absolute_pointer_ptr.
            //Mark the instance invalid by setting the pointer_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on absolute_pointer_ptr to continue to resolve
            //and return error based on observing pointer_handler is null.
            debugln!(DEBUG_ERROR, "Failed to uninstall absolute_pointer interface, status: {:x?}", status);

            unsafe {
                (*absolute_pointer_ptr).pointer_handler = ptr::null_mut();
            }
            //return without tearing down the context.
            status?;
        }

        let wait_for_input_event: efi::Handle = unsafe { (*absolute_pointer_ptr).absolute_pointer.wait_for_input };
        let status = static_boot_services().close_event(wait_for_input_event);
        if status.is_err() {
            //An error here means the event was not uninstalled, so in theory the notification_callback on it could still be
            //fired.
            //Mark the instance invalid by setting the pointer_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on absolute_pointer_ptr to continue to resolve
            //and return error based on observing pointer_handler is null.
            debugln!(DEBUG_ERROR, "Failed to close absolute_pointer.wait_for_input event, status: {:x?}", status);
            unsafe {
                (*absolute_pointer_ptr).pointer_handler = ptr::null_mut();
            }
            status?;
        }

        // None of the parts of absolute pointer are in use, so it is safe to reclaim it.
        drop(unsafe { Box::from_raw(absolute_pointer_ptr) });
        Ok(())
    }

    // event handler for wait_for_pointer event that is part of the absolute pointer interface.
    extern "efiapi" fn wait_for_pointer(event: efi::Event, context: *mut c_void) {
        let pointer_ctx = unsafe { (context as *mut PointerContext).as_mut().expect("bad context") };
        // raise to notify to protect access to pointer_handler, and check if event should be signalled.
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        {
            let pointer_handler = unsafe { pointer_ctx.pointer_handler.as_mut() };
            if let Some(pointer_handler) = pointer_handler {
                if pointer_handler.state_changed {
                    let _ = static_boot_services().signal_event(event);
                }
            } else {
                // implies that this API was invoked after pointer handler was dropped.
                debugln!(DEBUG_ERROR, "absolute_pointer_reset invoked after pointer dropped.");
            }
        }
        static_boot_services().restore_tpl(old_tpl);
    }

    // resets the pointer state - part of the absolute pointer interface.
    extern "efiapi" fn absolute_pointer_reset(
        this: *mut protocols::absolute_pointer::Protocol,
        _extended_verification: bool,
    ) -> efi::Status {
        if this.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let pointer_ctx = unsafe { (this as *mut PointerContext).as_mut().expect("bad context") };
        let mut status = efi::Status::SUCCESS;
        {
            // raise to notify to protect access to pointer_handler and reset pointer handler state
            let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);

            let pointer_handler = unsafe { pointer_ctx.pointer_handler.as_mut() };
            if let Some(pointer_handler) = pointer_handler {
                pointer_handler.reset_state();
            } else {
                // implies that this API was invoked after pointer handler was dropped.
                debugln!(DEBUG_ERROR, "absolute_pointer_reset invoked after pointer dropped.");
                status = efi::Status::DEVICE_ERROR;
            }

            static_boot_services().restore_tpl(old_tpl);
        }
        status
    }

    // returns the current pointer state in the `state` buffer provided by the caller - part of the absolute pointer
    // interface.
    extern "efiapi" fn absolute_pointer_get_state(
        this: *mut protocols::absolute_pointer::Protocol,
        state: *mut protocols::absolute_pointer::State,
    ) -> efi::Status {
        if state.is_null() || state.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }

        let pointer_ctx = unsafe { (this as *mut PointerContext).as_mut().expect("bad context") };
        let mut status = efi::Status::SUCCESS;
        {
            // raise to notify to protect access to pointer_handler, and retrieve pointer handler state.
            let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);

            let pointer_handler = unsafe { pointer_ctx.pointer_handler.as_mut() };
            if let Some(pointer_handler) = pointer_handler {
                if pointer_handler.state_changed {
                    unsafe {
                        state.write(pointer_handler.current_state);
                    }
                    pointer_handler.state_changed = false;
                } else {
                    status = efi::Status::NOT_READY;
                }
            } else {
                // implies that this API was invoked after pointer handler was dropped.
                debugln!(DEBUG_ERROR, "absolute_pointer_get_state invoked after pointer dropped.");
                status = efi::Status::DEVICE_ERROR;
            }

            static_boot_services().restore_tpl(old_tpl);
        }
        status
    }
}

