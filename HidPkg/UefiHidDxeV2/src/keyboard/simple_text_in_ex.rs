//! Simple Text In Ex Protocol FFI Support.
//!
//! This module manages the Simple Text In Ex FFI.
//!
//! ## License
//!
//! Copyright (c) Microsoft Corporation. All rights reserved.
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
use alloc::{boxed::Box, vec::Vec};
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
/// once initialization is complete - for this reason the context structure includes a direct reference to static_boot_services()
/// so that TPL can be enforced without access to the *mut KeyboardHidHandler.
///
/// In addition, the simple_text_in_ex protocol element needs to be the first element in the structure so that the full
/// structure can be recovered by simple casting for simple_text_in FFI interfaces that only receive a pointer to the
/// simple_text_in_ex protocol structure.
#[repr(C)]
pub struct SimpleTextInExFfi {
    simple_text_in_ex: protocols::simple_text_input_ex::Protocol,
    key_notify_event: efi::Event,
    keyboard_handler: *mut KeyboardHidHandler,
}

impl SimpleTextInExFfi {
    /// Installs the simple text in ex protocol
    pub fn install(controller: efi::Handle, keyboard_handler: &mut KeyboardHidHandler) -> Result<(), efi::Status> {
        //Create simple_text_in context
        let simple_text_in_ex_ctx = SimpleTextInExFfi {
            simple_text_in_ex: protocols::simple_text_input_ex::Protocol {
                reset: Self::simple_text_in_ex_reset,
                read_key_stroke_ex: Self::simple_text_in_ex_read_key_stroke,
                wait_for_key_ex: ptr::null_mut(),
                set_state: Self::simple_text_in_ex_set_state,
                register_key_notify: Self::simple_text_in_ex_register_key_notify,
                unregister_key_notify: Self::simple_text_in_ex_unregister_key_notify,
            },
            key_notify_event: ptr::null_mut(),
            keyboard_handler: keyboard_handler as *mut KeyboardHidHandler,
        };

        let simple_text_in_ex_ptr = Box::into_raw(Box::new(simple_text_in_ex_ctx));

        //create event for wait_for_key
        let wait_for_key_event = unsafe {
            static_boot_services().create_event_unchecked(
                EventType::NOTIFY_WAIT,
                Tpl::NOTIFY,
                Some(Self::simple_text_in_ex_wait_for_key),
                simple_text_in_ex_ptr as *mut c_void,
            )
        };
        if wait_for_key_event.is_err() {
            drop(unsafe { Box::from_raw(simple_text_in_ex_ptr) });
            wait_for_key_event?;
        }
        let wait_for_key_event = wait_for_key_event.unwrap();

        unsafe { (*simple_text_in_ex_ptr).simple_text_in_ex.wait_for_key_ex = wait_for_key_event };

        //Key notifies are required to dispatch at TPL_CALLBACK per UEFI spec 2.10 section 12.2.5. The keyboard handler
        //interfaces run at TPL_NOTIFY and issue a static_boot_services().signal_event() on this event to pend key notifies to be
        //serviced at TPL_CALLBACK.
        let key_notify_event = unsafe {
            static_boot_services().create_event_unchecked(
                EventType::NOTIFY_SIGNAL,
                Tpl::CALLBACK,
                Some(Self::process_key_notifies),
                simple_text_in_ex_ptr as *mut c_void,
            )
        };
        if key_notify_event.is_err() {
            let _ = static_boot_services().close_event(wait_for_key_event);
            drop(unsafe { Box::from_raw(simple_text_in_ex_ptr) });
            key_notify_event?;
        }
        let key_notify_event = key_notify_event.unwrap();
        unsafe { (*simple_text_in_ex_ptr).key_notify_event = key_notify_event };

        //install the simple_text_in_ex protocol
        let status = unsafe {
            static_boot_services().install_protocol_interface_unchecked(
                Some(controller),
                &r_efi::protocols::simple_text_input_ex::PROTOCOL_GUID,
                simple_text_in_ex_ptr as *mut c_void,
            )
        };

        if status.is_err() {
            let _ = static_boot_services().close_event(wait_for_key_event);
            let _ = static_boot_services().close_event(key_notify_event);
            drop(unsafe { Box::from_raw(simple_text_in_ex_ptr) });
            status?;
        }

        Ok(())
    }

    /// Uninstalls the simple text in ex protocol
    pub fn uninstall(agent: efi::Handle, controller: efi::Handle) -> Result<(), efi::Status> {
        //Controller is set - that means initialize() was called, and there is potential state exposed thru FFI that needs
        //to be cleaned up.
        let status = unsafe {
            static_boot_services().open_protocol_unchecked(
                controller,
                &r_efi::protocols::simple_text_input_ex::PROTOCOL_GUID,
                agent,
                controller,
                efi::OPEN_PROTOCOL_GET_PROTOCOL,
            )
        };
        if status.is_err() {
            //No protocol is actually installed on this controller, so nothing to clean up.
            return Ok(());
        }
        let simple_text_in_ex_ptr: *mut SimpleTextInExFfi = status.unwrap() as *mut SimpleTextInExFfi;

        //Attempt to uninstall the simple_text_in interface - this should disconnect any drivers using it and release
        //the interface.
        let status = unsafe {
            static_boot_services().uninstall_protocol_interface_unchecked(
                controller,
                &r_efi::protocols::simple_text_input_ex::PROTOCOL_GUID,
                simple_text_in_ex_ptr as *mut c_void,
            )
        };

        if status.is_err() {
            //An error here means some other driver might be holding on to the simple_text_in_ptr.
            //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on absolute_pointer_ptr to continue to resolve
            //and return error based on observing keyboard_handler is null.
            debugln!(DEBUG_ERROR, "Failed to uninstall simple_text_in interface, status: {:x?}", status);

            unsafe {
                (*simple_text_in_ex_ptr).keyboard_handler = ptr::null_mut();
            }
            //return without tearing down the context.
            status?;
        }

        let wait_for_key_event: efi::Handle = unsafe { (*simple_text_in_ex_ptr).simple_text_in_ex.wait_for_key_ex };
        let status = static_boot_services().close_event(wait_for_key_event);
        if status.is_err() {
            //An error here means the event was not closed, so in theory the notification_callback on it could still be
            //fired.
            //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on simple_text_in_ptr to continue to resolve
            //and return error based on observing keyboard_handler is null.
            debugln!(DEBUG_ERROR, "Failed to close simple_text_in_ptr.wait_for_key event, status: {:x?}", status);
            unsafe {
                (*simple_text_in_ex_ptr).keyboard_handler = ptr::null_mut();
            }
            status?;
        }

        let key_notify_event: efi::Handle = unsafe { (*simple_text_in_ex_ptr).key_notify_event };
        let status = static_boot_services().close_event(key_notify_event);
        if status.is_err() {
            //An error here means the event was not closed, so in theory the notification_callback on it could still be
            //fired.
            //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the PointerContext
            //instance. Leaking context allows calls through the pointers on simple_text_in_ptr to continue to resolve
            //and return error based on observing keyboard_handler is null.
            debugln!(DEBUG_ERROR, "Failed to close key_notify_event event, status: {:x?}", status);
            unsafe {
                (*simple_text_in_ex_ptr).keyboard_handler = ptr::null_mut();
            }
            status?;
        }

        // None of the parts of simple_text_in_ptr simple_text_in_ptr are in use, so it is safe to reclaim it.
        drop(unsafe { Box::from_raw(simple_text_in_ex_ptr) });
        Ok(())
    }

    // resets the keyboard state - part of the simple_text_in_ex protocol interface.
    extern "efiapi" fn simple_text_in_ex_reset(
        this: *mut protocols::simple_text_input_ex::Protocol,
        extended_verification: efi::Boolean,
    ) -> efi::Status {
        if this.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let status = 'reset_processing: {
            let Some(keyboard_handler) = (unsafe { context.keyboard_handler.as_mut() }) else {
                break 'reset_processing efi::Status::DEVICE_ERROR;
            };
            let Some(controller) = keyboard_handler.controller() else {
                break 'reset_processing efi::Status::DEVICE_ERROR;
            };
            UefiHidIoFactory::new(keyboard_handler.agent())
                .new_hid_io(controller, false)
                .and_then(|hid_io| keyboard_handler.reset(hid_io.as_ref(), extended_verification.into()))
                .err()
                .unwrap_or(efi::Status::SUCCESS)
        };
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // reads a key stroke - part of the simple_text_in_ex protocol interface.
    extern "efiapi" fn simple_text_in_ex_read_key_stroke(
        this: *mut protocols::simple_text_input_ex::Protocol,
        key_data: *mut protocols::simple_text_input_ex::KeyData,
    ) -> efi::Status {
        if this.is_null() || key_data.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let status = 'read_key_stroke: {
            let keyboard_handler = unsafe { context.keyboard_handler.as_mut() };
            let Some(keyboard_handler) = keyboard_handler else {
                break 'read_key_stroke efi::Status::DEVICE_ERROR;
            };
            if let Some(key) = keyboard_handler.pop_key() {
                unsafe { ptr::write(key_data, key) }
                efi::Status::SUCCESS
            } else {
                let key = protocols::simple_text_input_ex::KeyData {
                    key_state: keyboard_handler.get_key_state(),
                    ..Default::default()
                };
                unsafe { ptr::write(key_data, key) };
                efi::Status::NOT_READY
            }
        };
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // sets the keyboard state - part of the simple_text_in_ex protocol interface.
    extern "efiapi" fn simple_text_in_ex_set_state(
        this: *mut protocols::simple_text_input_ex::Protocol,
        key_toggle_state: *mut protocols::simple_text_input_ex::KeyToggleState,
    ) -> efi::Status {
        if this.is_null() || key_toggle_state.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let status = 'set_state_processing: {
            let Some(keyboard_handler) = (unsafe { context.keyboard_handler.as_mut() }) else {
                break 'set_state_processing efi::Status::DEVICE_ERROR;
            };
            let Some(controller) = keyboard_handler.controller() else {
                break 'set_state_processing efi::Status::DEVICE_ERROR;
            };
            let Ok(hid_io) = UefiHidIoFactory::new(keyboard_handler.agent()).new_hid_io(controller, false) else {
                break 'set_state_processing efi::Status::DEVICE_ERROR;
            };
            keyboard_handler.set_key_toggle_state(unsafe { key_toggle_state.read() });
            keyboard_handler.update_leds(hid_io.as_ref()).err().unwrap_or(efi::Status::SUCCESS)
        };
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // registers a key notification callback function - part of the simple_text_in_ex protocol interface.
    extern "efiapi" fn simple_text_in_ex_register_key_notify(
        this: *mut protocols::simple_text_input_ex::Protocol,
        key_data_ptr: *mut protocols::simple_text_input_ex::KeyData,
        key_notification_function: protocols::simple_text_input_ex::KeyNotifyFunction,
        notify_handle: *mut *mut c_void,
    ) -> efi::Status {
        if this.is_null()
            || key_data_ptr.is_null()
            || notify_handle.is_null()
            || key_notification_function as usize == 0
        {
            return efi::Status::INVALID_PARAMETER;
        }

        let context = unsafe { (this as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let status = {
            if let Some(keyboard_handler) = unsafe { context.keyboard_handler.as_mut() } {
                let key_data = unsafe { key_data_ptr.read() };
                let handle = keyboard_handler.insert_key_notify_callback(key_data, key_notification_function);
                unsafe { notify_handle.write(handle as *mut c_void) };
                efi::Status::SUCCESS
            } else {
                efi::Status::DEVICE_ERROR
            }
        };
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // Unregisters a key notification callback function - part of the simple_text_in_ex protocol interface.
    extern "efiapi" fn simple_text_in_ex_unregister_key_notify(
        this: *mut protocols::simple_text_input_ex::Protocol,
        notification_handle: *mut c_void,
    ) -> efi::Status {
        if this.is_null() {
            return efi::Status::INVALID_PARAMETER;
        }
        let context = unsafe { (this as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
        let status = if let Some(keyboard_handler) = unsafe { context.keyboard_handler.as_mut() } {
            keyboard_handler
                .remove_key_notify_callback(notification_handle as usize)
                .err()
                .unwrap_or(efi::Status::SUCCESS)
        } else {
            efi::Status::DEVICE_ERROR
        };
        static_boot_services().restore_tpl(old_tpl);
        status
    }

    // Event handler function for the wait_for_key_event
    extern "efiapi" fn simple_text_in_ex_wait_for_key(event: efi::Event, context: *mut c_void) {
        if context.is_null() {
            debugln!(DEBUG_ERROR, "simple_text_in_ex_wait_for_key invoked with invalid context");
            return;
        }
        let context = unsafe { (context as *mut SimpleTextInExFfi).as_mut() }.expect("bad pointer");

        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
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
        static_boot_services().restore_tpl(old_tpl);
    }

    // Event callback function for handling registered key notifications. Iterates over the queue of keys to be notified,
    // and invokes the registered callback function for each of those keys.
    extern "efiapi" fn process_key_notifies(_event: efi::Event, context: *mut c_void) {
        let Some(context) = (unsafe { (context as *mut SimpleTextInExFfi).as_mut() }) else {
            return;
        };
        loop {
            let mut pending_key = None;
            let mut pending_callbacks = Vec::new();

            let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);
            if let Some(keyboard_handler) = unsafe { context.keyboard_handler.as_mut() } {
                (pending_key, pending_callbacks) = keyboard_handler.pending_callbacks();
            } else {
                debugln!(DEBUG_ERROR, "process_key_notifies event called without a valid keyboard_handler");
            }
            static_boot_services().restore_tpl(old_tpl);

            //dispatch notifies (if any) at the TPL this event callback was invoked at.
            if let Some(mut pending_key) = pending_key {
                let key_ptr = &mut pending_key as *mut protocols::simple_text_input_ex::KeyData;
                for callback in pending_callbacks {
                    let _ = callback(key_ptr);
                }
            } else {
                // no pending notifies to process
                break;
            }
        }
    }
}
