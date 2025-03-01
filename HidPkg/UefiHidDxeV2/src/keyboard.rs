//! Provides Keyboard HID support.
//!
//! This module handles the core logic for processing keystrokes from HID
//! devices.
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
mod key_queue;
mod simple_text_in;
mod simple_text_in_ex;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    vec,
    vec::Vec,
};
use core::{ffi::c_void, ptr};

use r_efi::{efi, hii, protocols};

use hidparser::{
    report_data_types::{ReportId, Usage},
    ArrayField, ReportDescriptor, ReportField, VariableField,
};
use rust_advanced_logger_dxe::{debugln, function, DEBUG_ERROR, DEBUG_VERBOSE, DEBUG_WARN};

use boot_services::{event::EventType, tpl::Tpl, BootServices};

use crate::{
    hid_io::{HidIo, HidReportReceiver},
    keyboard::key_queue::OrdKeyData,
    static_boot_services,
};

// usages supported by this module
const KEYBOARD_MODIFIER_USAGE_MIN: u32 = 0x000700E0;
const KEYBOARD_MODIFIER_USAGE_MAX: u32 = 0x000700E7;
const KEYBOARD_USAGE_MIN: u32 = 0x00070001;
const KEYBOARD_USAGE_MAX: u32 = 0x00070065;
const LED_USAGE_MIN: u32 = 0x00080001;
const LED_USAGE_MAX: u32 = 0x00080005;

// maps a given field to a routine that handles input from it.
#[derive(Debug, Clone)]
struct ReportFieldWithHandler<T> {
    field: T,
    report_handler: fn(handler: &mut KeyboardHidHandler, field: T, report: &[u8]),
}

// maps a given field to a routine that builds output reports from it.
#[derive(Debug, Clone)]
struct ReportFieldBuilder<T> {
    field: T,
    field_builder: fn(&mut KeyboardHidHandler, field: T, report: &mut [u8]),
}

// Defines an input report and the fields of interest in it.
#[derive(Debug, Default, Clone)]
struct KeyboardReportData {
    report_id: Option<ReportId>,
    report_size: usize,
    relevant_variable_fields: Vec<ReportFieldWithHandler<VariableField>>,
    relevant_array_fields: Vec<ReportFieldWithHandler<ArrayField>>,
}

// Defines an output report and the fields of interest in it.
#[derive(Debug, Default, Clone)]
struct KeyboardOutputReportBuilder {
    report_id: Option<ReportId>,
    report_size: usize,
    relevant_variable_fields: Vec<ReportFieldBuilder<VariableField>>,
}

#[repr(C)]
struct LayoutChangeContext {
    keyboard_handler: *mut KeyboardHidHandler,
}

/// Keyboard HID Handler
pub struct KeyboardHidHandler {
    agent: efi::Handle,
    controller: Option<efi::Handle>,
    input_reports: BTreeMap<Option<ReportId>, KeyboardReportData>,
    output_builders: Vec<KeyboardOutputReportBuilder>,
    report_id_present: bool,
    last_keys: BTreeSet<Usage>,
    current_keys: BTreeSet<Usage>,
    led_state: BTreeSet<Usage>,
    key_queue: key_queue::KeyQueue,
    notification_callbacks: BTreeMap<usize, (OrdKeyData, protocols::simple_text_input_ex::KeyNotifyFunction)>,
    next_notify_handle: usize,
    key_notify_event: efi::Event,
    layout_change_event: efi::Event,
    layout_context: *mut LayoutChangeContext,
}

impl KeyboardHidHandler {
    /// Instantiates a new Keyboard HID handler. `agent` is the handle that owns the handler (typically image_handle)
    pub fn new(agent: efi::Handle) -> Self {
        Self {
            agent,
            controller: None,
            input_reports: BTreeMap::new(),
            output_builders: Vec::new(),
            report_id_present: false,
            last_keys: BTreeSet::new(),
            current_keys: BTreeSet::new(),
            led_state: BTreeSet::new(),
            key_queue: Default::default(),
            notification_callbacks: BTreeMap::new(),
            next_notify_handle: 0,
            key_notify_event: core::ptr::null_mut(),
            layout_change_event: core::ptr::null_mut(),
            layout_context: core::ptr::null_mut(),
        }
    }

    // Processes the report descriptor to determine whether this is a supported device, and if so, extract the information
    // required to process reports.
    fn process_descriptor(&mut self, descriptor: ReportDescriptor) -> Result<(), efi::Status> {
        let multiple_reports =
            descriptor.input_reports.len() > 1 || descriptor.output_reports.len() > 1 || descriptor.features.len() > 1;

        for report in &descriptor.input_reports {
            let mut report_data = KeyboardReportData { report_id: report.report_id, ..Default::default() };

            self.report_id_present = report.report_id.is_some();

            if multiple_reports && !self.report_id_present {
                //Invalid to have None ReportId if multiple reports present.
                Err(efi::Status::DEVICE_ERROR)?;
            }

            report_data.report_size = report.size_in_bits.div_ceil(8);

            for field in &report.fields {
                match field {
                    //Variable fields (typically used for modifier Usages)
                    ReportField::Variable(field) => {
                        if let KEYBOARD_MODIFIER_USAGE_MIN..=KEYBOARD_MODIFIER_USAGE_MAX = field.usage.into() {
                            report_data.relevant_variable_fields.push(ReportFieldWithHandler::<VariableField> {
                                field: field.clone(),
                                report_handler: Self::handle_variable_key,
                            });
                        }
                    }
                    //Array fields (typically used for key strokes)
                    ReportField::Array(field) => {
                        for usage_list in &field.usage_list {
                            if usage_list.contains(Usage::from(KEYBOARD_USAGE_MIN))
                                || usage_list.contains(Usage::from(KEYBOARD_USAGE_MAX))
                            {
                                report_data.relevant_array_fields.push(ReportFieldWithHandler::<ArrayField> {
                                    field: field.clone(),
                                    report_handler: Self::handle_array_key,
                                });
                                break;
                            }
                        }
                    }
                    ReportField::Padding(_) => (), // padding irrelevant.
                }
            }
            if !(report_data.relevant_variable_fields.is_empty() && report_data.relevant_array_fields.is_empty()) {
                self.input_reports.insert(report_data.report_id, report_data);
            }
        }

        for report in &descriptor.output_reports {
            let mut report_builder = KeyboardOutputReportBuilder { report_id: report.report_id, ..Default::default() };

            self.report_id_present = report.report_id.is_some();

            if multiple_reports && !self.report_id_present {
                //invalid to have None ReportId if multiple reports present.
                Err(efi::Status::DEVICE_ERROR)?;
            }

            report_builder.report_size = usize::div_ceil(report.size_in_bits, 8);

            for field in &report.fields {
                match field {
        //Variable fields in output reports (typically used for LEDs).
        ReportField::Variable(field) => {
          if let LED_USAGE_MIN..=LED_USAGE_MAX = field.usage.into() {
            report_builder.relevant_variable_fields.push(
              ReportFieldBuilder {
                field: field.clone(),
                field_builder: Self::build_led_report
              }
            )
          }
        },
        ReportField::Array(_) | // No support for array field report outputs; could be added if required.
        ReportField::Padding(_) => (), // padding fields irrelevant.
      }
            }
            if !report_builder.relevant_variable_fields.is_empty() {
                self.output_builders.push(report_builder);
            }
        }

        if self.input_reports.is_empty() && self.output_builders.is_empty() {
            Err(efi::Status::UNSUPPORTED)
        } else {
            Ok(())
        }
    }

    // Helper routine to handle variable keyboard input report fields
    fn handle_variable_key(&mut self, field: VariableField, report: &[u8]) {
        match field.field_value(report) {
            Some(x) if x != 0 => _ = self.current_keys.insert(field.usage),
            _ => (),
        }
    }

    // Helper routine to handle array keyboard input report fields
    fn handle_array_key(&mut self, field: ArrayField, report: &[u8]) {
        match field.field_value(report) {
            Some(index) if index != 0 => {
                let mut index = (index as u32 - u32::from(field.logical_minimum)) as usize;
                let usage = field.usage_list.iter().find_map(|x| {
                    let range_size = (x.end() - x.start()) as usize;
                    if index <= range_size {
                        x.range().nth(index)
                    } else {
                        index -= range_size;
                        None
                    }
                });
                if let Some(usage) = usage {
                    self.current_keys.insert(Usage::from(usage));
                }
            }
            _ => (),
        }
    }

    // Helper routine that updates the fields in the given report buffer for the given field (called for each field for
    // every LED usage that was discovered in the output report descriptor).
    fn build_led_report(&mut self, field: VariableField, report: &mut [u8]) {
        let status = field.set_field_value(self.led_state.contains(&field.usage).into(), report);
        if status.is_err() {
            debugln!(DEBUG_WARN, "{:}: failed to set field value: {:?}", function!(), status);
        }
    }

    // Generates LED output reports - usually there is only one, but this implementation handles an arbitrary number of
    // possible output reports. If more than one output report is defined, all will be sent whenever there is a change in
    // LEDs.
    fn generate_led_output_reports(&mut self) -> Vec<(Option<ReportId>, Vec<u8>)> {
        let mut output_vec = Vec::new();
        let current_leds: BTreeSet<Usage> = self.key_queue.active_leds().iter().cloned().collect();
        if current_leds != self.led_state {
            self.led_state = current_leds;
            for output_builder in self.output_builders.clone() {
                let mut report_buffer = vec![0u8; output_builder.report_size];
                for field_builder in &output_builder.relevant_variable_fields {
                    (field_builder.field_builder)(self, field_builder.field.clone(), report_buffer.as_mut_slice());
                }
                output_vec.push((output_builder.report_id, report_buffer));
            }
        }
        output_vec
    }

    // Installs the FFI interfaces that provide keyboard support to the rest of the system
    fn install_protocol_interfaces(&mut self, controller: efi::Handle) -> Result<(), efi::Status> {
        simple_text_in::SimpleTextInFfi::install(controller, self)?;
        simple_text_in_ex::SimpleTextInExFfi::install(controller, self)?;
        self.controller = Some(controller);

        // after this point, access to self must be guarded by raising TPL to NOTIFY.
        Ok(())
    }

    // Installs an event to be notified when a new layout is installed. This allows the driver to respond dynamically to
    // installation of new layouts and handle keys accordingly.
    fn install_layout_change_event(&mut self) -> Result<(), efi::Status> {
        let context = LayoutChangeContext { keyboard_handler: self as *mut Self };
        let context_ptr: *mut LayoutChangeContext = Box::into_raw(Box::new(context));

        let event = unsafe {
            static_boot_services().create_event_ex_unchecked(
                EventType::NOTIFY_SIGNAL,
                Tpl::NOTIFY,
                on_layout_update,
                context_ptr as *mut c_void,
                &protocols::hii_database::SET_KEYBOARD_LAYOUT_EVENT_GUID,
            )?
        };

        self.layout_change_event = event;
        self.layout_context = context_ptr;

        Ok(())
    }

    // Closes the layout change event.
    fn uninstall_layout_change_event(&mut self) -> Result<(), efi::Status> {
        if !self.layout_change_event.is_null() {
            let layout_change_event: efi::Handle = self.layout_change_event;
            let status = static_boot_services().close_event(layout_change_event);
            if status.is_err() {
                //An error here means the event was not closed, so in theory the notification_callback on it could still be fired.
                //Mark the instance invalid by setting the keyboard_handler raw pointer to null, but leak the LayoutContext
                //instance. Leaking context allows context usage in the callback should it fire.
                debugln!(DEBUG_ERROR, "Failed to close layout_change_event event, status: {:x?}", status);
                unsafe {
                    (*self.layout_context).keyboard_handler = ptr::null_mut();
                }
                return status;
            }
            // safe to drop layout change context.
            drop(unsafe { Box::from_raw(self.layout_context) });
            self.layout_context = ptr::null_mut();
            self.layout_change_event = ptr::null_mut();
        }
        Ok(())
    }

    // Installs a default keyboard layout.
    fn install_default_layout(&mut self) -> Result<(), efi::Status> {
        let status = unsafe { static_boot_services().locate_protocol::<r_efi::protocols::hii_database::Protocol>(None) };
        if status.is_err() {
            let status_code = status.err().unwrap();
            debugln!(
                DEBUG_ERROR,
                "keyboard::install_default_layout: Could not locate hii_database protocol to install keyboard layout: {:x?}",
                status_code
            );
            return Err(status_code);
        }
        let hii_database_protocol = status.unwrap();
        let hii_database_protocol_ptr = hii_database_protocol as *mut protocols::hii_database::Protocol;

        let mut hii_handle: hii::Handle = ptr::null_mut();
        let status = (hii_database_protocol.new_package_list)(
            hii_database_protocol_ptr,
            hii_keyboard_layout::get_default_keyboard_pkg_list_buffer().as_ptr() as *const hii::PackageListHeader,
            ptr::null_mut(),
            ptr::addr_of_mut!(hii_handle),
        );

        if status.is_error() {
            debugln!(
                DEBUG_ERROR,
                "keyboard::install_default_layout: Failed to install keyboard layout package: {:x?}",
                status
            );
            Err(status)?;
        }

        let status = (hii_database_protocol.set_keyboard_layout)(
            hii_database_protocol_ptr,
            &hii_keyboard_layout::DEFAULT_KEYBOARD_LAYOUT_GUID as *const efi::Guid as *mut efi::Guid,
        );
        if status.is_error() {
            debugln!(DEBUG_ERROR, "keyboard::install_default_layout: Failed to set keyboard layout: {:x?}", status);
            Err(status)?;
        }

        Ok(())
    }

    // Initializes the keyboard layout. If no layout is already installed in the system, a default layout is installed.
    fn initialize_keyboard_layout(&mut self) -> Result<(), efi::Status> {
        self.install_layout_change_event()?;

        //fake signal event to pick up any existing layout
        on_layout_update(self.layout_change_event, self.layout_context as *mut c_void);

        //install a default layout if no layout is installed.
        if self.key_queue.layout().is_none() {
            self.install_default_layout()?;
        }
        Ok(())
    }

    /// Resets the keyboard driver state. Clears any pending key state. `extended verification` will also reset toggle
    /// state.
    pub fn reset(&mut self, hid_io: &dyn HidIo, extended_verification: bool) -> Result<(), efi::Status> {
        self.last_keys.clear();
        self.current_keys.clear();
        self.key_queue.reset(extended_verification);
        if extended_verification {
            self.update_leds(hid_io)?;
        }
        Ok(())
    }

    /// Called to send LED state to the device if there has been a change in LEDs.
    pub fn update_leds(&mut self, hid_io: &dyn HidIo) -> Result<(), efi::Status> {
        for (id, output_report) in self.generate_led_output_reports() {
            let result = hid_io.set_output_report(id.map(|x| u32::from(x) as u8), &output_report);
            if let Err(result) = result {
                debugln!(DEBUG_ERROR, "unexpected error sending output report: {:?}", result);
                return Err(result);
            }
        }
        Ok(())
    }

    /// Returns a clone of the keystroke at the front of the keystroke queue.
    pub fn peek_key(&mut self) -> Option<protocols::simple_text_input_ex::KeyData> {
        self.key_queue.peek_key()
    }

    /// Removes and returns the keystroke at the front of the keystroke queue.
    pub fn pop_key(&mut self) -> Option<protocols::simple_text_input_ex::KeyData> {
        self.key_queue.pop_key()
    }

    /// Returns the current key state (i.e. the SHIFT and TOGGLE state).
    pub fn get_key_state(&mut self) -> protocols::simple_text_input_ex::KeyState {
        self.key_queue.init_key_state()
    }

    /// Sets the current key state.
    pub fn set_key_toggle_state(&mut self, toggle_state: u8) {
        self.key_queue.set_key_toggle_state(toggle_state);
        self.generate_led_output_reports();
    }

    /// Registers a new key notify callback function to be invoked on the specified `key_data` press.
    ///
    /// Returns a handle that is used to unregister the callback if desired.
    pub fn insert_key_notify_callback(
        &mut self,
        key_data: protocols::simple_text_input_ex::KeyData,
        key_notification_function: protocols::simple_text_input_ex::KeyNotifyFunction,
    ) -> usize {
        let key_data = OrdKeyData(key_data);
        for (handle, entry) in &self.notification_callbacks {
            if entry.0 == key_data && entry.1 == key_notification_function {
                //this callback already exists for this key, so return the current handle.
                return *handle;
            }
        }
        // key_data/callback combo doesn't exist, create a new registration for it.
        self.next_notify_handle += 1;
        self.notification_callbacks.insert(self.next_notify_handle, (key_data.clone(), key_notification_function));
        self.key_queue.add_notify_key(key_data);
        self.next_notify_handle
    }

    /// Unregisters a previously registered key notify callback function.
    pub fn remove_key_notify_callback(&mut self, notification_handle: usize) -> Result<(), efi::Status> {
        if let Some(entry) = self.notification_callbacks.remove(&notification_handle) {
            let removed_key = entry.0;
            if !self.notification_callbacks.values().any(|(key, _)| *key == removed_key) {
                // no other handlers exist for the key in the removed entry, so remove it from the key_queue as well.
                self.key_queue.remove_notify_key(&removed_key);
            }
            Ok(())
        } else {
            Err(efi::Status::INVALID_PARAMETER)
        }
    }

    /// Returns the set of keys that have pending callbacks, along with the vector of callback functions associated with
    /// each key.
    pub fn pending_callbacks(
        &mut self,
    ) -> (Option<protocols::simple_text_input_ex::KeyData>, Vec<protocols::simple_text_input_ex::KeyNotifyFunction>)
    {
        if let Some(pending_notify_key) = self.key_queue.pop_notify_key() {
            let mut pending_callbacks = Vec::new();
            for (key, callback) in self.notification_callbacks.values() {
                if OrdKeyData(pending_notify_key).matches_registered_key(key) {
                    pending_callbacks.push(*callback);
                }
            }
            (Some(pending_notify_key), pending_callbacks)
        } else {
            (None, Vec::new())
        }
    }

    /// Returns the agent associated with this KeyboardHidHandler
    pub fn agent(&self) -> efi::Handle {
        self.agent
    }

    /// Returns the controller associated with this KeyboardHidHandler.
    pub fn controller(&self) -> Option<efi::Handle> {
        self.controller
    }

    #[cfg(test)]
    pub fn set_controller(&mut self, controller: Option<efi::Handle>) {
        self.controller = controller;
    }

    #[cfg(test)]
    pub fn set_layout(&mut self, layout: Option<hii_keyboard_layout::HiiKeyboardLayout>) {
        self.key_queue.set_layout(layout)
    }
    #[cfg(test)]
    pub fn set_notify_event(&mut self, event: efi::Event) {
        self.key_notify_event = event;
    }
}

impl HidReportReceiver for KeyboardHidHandler {
    fn initialize(&mut self, controller: efi::Handle, hid_io: &dyn HidIo) -> Result<(), efi::Status> {
        let descriptor = hid_io.get_report_descriptor()?;
        self.process_descriptor(descriptor)?;
        // Set the key toggle state here so that the subsequent reset() can send the LED state to the device.
        self.set_key_toggle_state(protocols::simple_text_input_ex::CAPS_LOCK_ACTIVE);
        self.reset(hid_io, true)?;
        self.install_protocol_interfaces(controller)?;
        self.initialize_keyboard_layout()?;
        Ok(())
    }

    fn receive_report(&mut self, report: &[u8], hid_io: &dyn HidIo) {
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);

        let mut output_reports = Vec::new();
        'report_processing: {
            if report.is_empty() {
                break 'report_processing;
            }
            // determine whether report includes report id byte and adjust the buffer as needed.
            let (report_id, report) = match self.report_id_present {
                true => (Some(ReportId::from(&report[0..1])), &report[1..]),
                false => (None, &report[0..]),
            };

            if report.is_empty() {
                break 'report_processing;
            }

            if let Some(report_data) = self.input_reports.get(&report_id).cloned() {
                if report.len() != report_data.report_size {
                    //Some devices report extra bytes in their reports. Warn about this, but try and process anyway.
                    debugln!(
                        DEBUG_VERBOSE,
                        "{:?}:{:?} unexpected report length for report_id: {:?}. expected {:?}, actual {:?}",
                        function!(),
                        line!(),
                        report_id,
                        report_data.report_size,
                        report.len()
                    );
                    debugln!(DEBUG_VERBOSE, "report: {:x?}", report);
                }

                //reset currently active keys to empty set.
                self.current_keys.clear();

                // hand the report data to the handler for each relevant field for field-specific processing.
                for field in report_data.relevant_variable_fields {
                    (field.report_handler)(self, field.field, report);
                }

                for field in report_data.relevant_array_fields {
                    (field.report_handler)(self, field.field, report);
                }

                //check if any key state has changed.
                if self.last_keys != self.current_keys {
                    // process keys that are not in both sets: that is the set of keys that have changed.
                    // XOR on the sets yields a set of keys that are in either last or current keys, but not both.

                    // Modifier keys need to be processed first so that normal key processing includes modifiers that showed up in
                    // the same report. The key sets are sorted by Usage, and modifier keys all have higher usages than normal keys
                    // - so use a reverse iterator to process the modifier keys first. In addition, all released keys should be
                    // processed first so that pressed keys (which typically generate key stroke events) have the most recent
                    // key state associated with them.
                    let mut released_keys = Vec::new();
                    let mut pressed_keys = Vec::new();
                    for changed_key in (&self.last_keys ^ &self.current_keys).into_iter().rev() {
                        if self.last_keys.contains(&changed_key) {
                            //In the last key list, but not in current. This is a key release.
                            released_keys.push(changed_key);
                        } else {
                            //Not in last, so must be in current. This is a key press.
                            pressed_keys.push(changed_key);
                        }
                    }
                    for key in released_keys {
                        self.key_queue.keystroke(key, key_queue::KeyAction::KeyUp);
                    }
                    for key in pressed_keys {
                        self.key_queue.keystroke(key, key_queue::KeyAction::KeyDown);
                    }

                    //after processing all the key strokes, check if any keys were pressed that should trigger the notifier callback
                    //and if so, signal the event to trigger notify processing at the appropriate TPL.
                    if self.key_queue.peek_notify_key().is_some() {
                        let _ = static_boot_services().signal_event(self.key_notify_event);
                    }

                    //after processing all the key strokes, send updated LED state if required.
                    output_reports = self.generate_led_output_reports();
                }
                //after all key handling is complete for this report, update the last key set to match the current key set.
                self.last_keys = self.current_keys.clone();
            }
        }

        static_boot_services().restore_tpl(old_tpl);

        // if any output reports, send them after releasing handler.
        for (id, output_report) in output_reports {
            let result = hid_io.set_output_report(id.map(|x| u32::from(x) as u8), &output_report);
            if let Err(result) = result {
                debugln!(DEBUG_ERROR, "unexpected error sending output report: {:?}", result);
            }
        }
    }
}

impl Drop for KeyboardHidHandler {
    fn drop(&mut self) {
        if let Some(controller) = self.controller {
            if let Err(status) = simple_text_in::SimpleTextInFfi::uninstall(self.agent, controller) {
                debugln!(DEBUG_ERROR, "KeyboardHidHandler::drop: Failed to uninstall simple_text_in: {:?}", status);
            }
            if let Err(status) = simple_text_in_ex::SimpleTextInExFfi::uninstall(self.agent, controller) {
                debugln!(DEBUG_ERROR, "KeyboardHidHandler::drop: Failed to uninstall simple_text_in: {:?}", status);
            }
        }
        if let Err(status) = self.uninstall_layout_change_event() {
            debugln!(DEBUG_ERROR, "KeyboardHidHandler::drop: Failed to close layout_change_event: {:?}", status);
        }
    }
}

// handles keyboard layout change event that occurs when a new keyboard layout is set.
extern "efiapi" fn on_layout_update(_event: efi::Event, context: *mut c_void) {
    let context = unsafe { (context as *mut LayoutChangeContext).as_mut() }.expect("bad context pointer");
    let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);

    'layout_processing: {
        if context.keyboard_handler.is_null() {
            debugln!(DEBUG_ERROR, "on_layout_update invoked with invalid handler");
            break 'layout_processing;
        }

        let keyboard_handler = unsafe { context.keyboard_handler.as_mut() }.expect("bad keyboard handler");

        let status = unsafe { static_boot_services().locate_protocol::<r_efi::protocols::hii_database::Protocol>(None) };

        if status.is_err() {
            //nothing to do if there is no hii protocol.
            break 'layout_processing;
        }

        let hii_database_protocol = status.unwrap();
        let hii_database_protocol_ptr = hii_database_protocol as *mut protocols::hii_database::Protocol;

        // retrieve keyboard layout size
        let mut layout_buffer_len: u16 = 0;
        match (hii_database_protocol.get_keyboard_layout)(
            hii_database_protocol_ptr,
            ptr::null_mut(),
            &mut layout_buffer_len as *mut u16,
            ptr::null_mut(),
        ) {
            efi::Status::NOT_FOUND => break 'layout_processing,
            status if status != efi::Status::BUFFER_TOO_SMALL => {
                debugln!(
                    DEBUG_ERROR,
                    "{:}: unexpected return from get_keyboard_layout when trying to determine length: {:x?}",
                    function!(),
                    status
                );
                break 'layout_processing;
            }
            _ => (),
        }

        let mut keyboard_layout_buffer = vec![0u8; layout_buffer_len as usize];
        let status = (hii_database_protocol.get_keyboard_layout)(
            hii_database_protocol_ptr,
            ptr::null_mut(),
            &mut layout_buffer_len as *mut u16,
            keyboard_layout_buffer.as_mut_ptr() as *mut protocols::hii_database::KeyboardLayout<0>,
        );

        if status.is_error() {
            debugln!(DEBUG_ERROR, "Unexpected return from get_keyboard_layout: {:x?}", status);
            break 'layout_processing;
        }

        let keyboard_layout = hii_keyboard_layout::keyboard_layout_from_buffer(&keyboard_layout_buffer);
        match keyboard_layout {
            Ok(keyboard_layout) => {
                keyboard_handler.key_queue.set_layout(Some(keyboard_layout));
            }
            Err(_) => {
                debugln!(DEBUG_ERROR, "keyboard::on_layout_update: Could not parse keyboard layout buffer.");
                break 'layout_processing;
            }
        }
    }

    static_boot_services().restore_tpl(old_tpl);
}
