//! Provides Pointer HID support.
//!
//! This module handles the core logic for processing pointer input from HID
//! devices.
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
mod absolute_pointer;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use r_efi::{efi, protocols};

use hidparser::{
    report_data_types::{ReportId, Usage},
    ReportDescriptor, ReportField, VariableField,
};
use rust_advanced_logger_dxe::{debugln, function, DEBUG_ERROR, DEBUG_VERBOSE};

use self::absolute_pointer::PointerContext;
use boot_services::{BootServices, tpl::Tpl};

use crate::{static_boot_services, hid_io::{HidIo, HidReportReceiver}};

// Usages supported by this module.
const GENERIC_DESKTOP_X: u32 = 0x00010030;
const GENERIC_DESKTOP_Y: u32 = 0x00010031;
const GENERIC_DESKTOP_Z: u32 = 0x00010032;
const GENERIC_DESKTOP_WHEEL: u32 = 0x00010038;
const BUTTON_MIN: u32 = 0x00090001;
const BUTTON_MAX: u32 = 0x00090020; //Per spec, the Absolute Pointer protocol supports a 32-bit button state field.
const DIGITIZER_SWITCH_MIN: u32 = 0x000d0042;
const DIGITIZER_SWITCH_MAX: u32 = 0x000d0046;

// number of points on the X/Y axis for this implementation.
const AXIS_RESOLUTION: u64 = 1024;
const CENTER: u64 = AXIS_RESOLUTION / 2;

// Maps a given field to a routine that handles input from it.
#[derive(Debug, Clone)]
struct ReportFieldWithHandler {
    field: VariableField,
    report_handler: fn(&mut PointerHidHandler, field: VariableField, report: &[u8]),
}

// Defines a report and the fields of interest within it.
#[derive(Debug, Default, Clone)]
struct PointerReportData {
    report_id: Option<ReportId>,
    report_size: usize,
    relevant_fields: Vec<ReportFieldWithHandler>,
}

/// Pointer HID Handler
pub struct PointerHidHandler {
    agent: efi::Handle,
    controller: Option<efi::Handle>,
    input_reports: BTreeMap<Option<ReportId>, PointerReportData>,
    supported_usages: BTreeSet<Usage>,
    report_id_present: bool,
    state_changed: bool,
    current_state: protocols::absolute_pointer::State,
}

impl PointerHidHandler {
    /// Instantiates a new Pointer HID handler. `agent` is the handle that owns the handler (typically image_handle).
    pub fn new(agent: efi::Handle) -> Self {
        let mut handler = Self {
            agent,
            controller: None,
            input_reports: BTreeMap::new(),
            supported_usages: BTreeSet::new(),
            report_id_present: false,
            state_changed: false,
            current_state: Default::default(),
        };
        handler.reset_state();
        handler
    }

    // Processes the report descriptor to determine whether this is a supported device, and if so, extract the information
    // required to process reports.
    fn process_descriptor(&mut self, descriptor: ReportDescriptor) -> Result<(), efi::Status> {
        let multiple_reports = descriptor.input_reports.len() > 1;

        for report in &descriptor.input_reports {
            let mut report_data = PointerReportData { report_id: report.report_id, ..Default::default() };

            self.report_id_present = report.report_id.is_some();

            if multiple_reports && !self.report_id_present {
                //invalid to have None ReportId if multiple reports present.
                Err(efi::Status::DEVICE_ERROR)?;
            }

            report_data.report_size = report.size_in_bits.div_ceil(8);

            for field in &report.fields {
                if let ReportField::Variable(field) = field {
                    match field.usage.into() {
                        GENERIC_DESKTOP_X => {
                            let field_handler =
                                ReportFieldWithHandler { field: field.clone(), report_handler: Self::x_axis_handler };
                            report_data.relevant_fields.push(field_handler);
                            self.supported_usages.insert(field.usage);
                        }
                        GENERIC_DESKTOP_Y => {
                            let field_handler =
                                ReportFieldWithHandler { field: field.clone(), report_handler: Self::y_axis_handler };
                            report_data.relevant_fields.push(field_handler);
                            self.supported_usages.insert(field.usage);
                        }
                        GENERIC_DESKTOP_Z | GENERIC_DESKTOP_WHEEL => {
                            let field_handler =
                                ReportFieldWithHandler { field: field.clone(), report_handler: Self::z_axis_handler };
                            report_data.relevant_fields.push(field_handler);
                            self.supported_usages.insert(field.usage);
                        }
                        BUTTON_MIN..=BUTTON_MAX => {
                            let field_handler =
                                ReportFieldWithHandler { field: field.clone(), report_handler: Self::button_handler };
                            report_data.relevant_fields.push(field_handler);
                            self.supported_usages.insert(field.usage);
                        }
                        DIGITIZER_SWITCH_MIN..=DIGITIZER_SWITCH_MAX => {
                            let field_handler =
                                ReportFieldWithHandler { field: field.clone(), report_handler: Self::button_handler };
                            report_data.relevant_fields.push(field_handler);
                            self.supported_usages.insert(field.usage);
                        }
                        _ => (), //other usages irrelevant
                    }
                }
            }

            if !report_data.relevant_fields.is_empty() {
                self.input_reports.insert(report_data.report_id, report_data);
            }
        }
        if !self.input_reports.is_empty() {
            Ok(())
        } else {
            Err(efi::Status::UNSUPPORTED)
        }
    }

    // Helper routine that handles projecting relative and absolute axis reports onto the fixed
    // absolute report axis that this driver produces.
    fn resolve_axis(current_value: u64, field: VariableField, report: &[u8]) -> Option<u64> {
        if field.attributes.relative {
            //for relative, just update and clamp the current state.
            let new_value = current_value as i64 + field.field_value(report)?;
            Some(new_value.clamp(0, AXIS_RESOLUTION as i64) as u64)
        } else {
            //for absolute, project onto 0..AXIS_RESOLUTION
            let mut new_value = field.field_value(report)?;

            //translate to zero.
            new_value = new_value.checked_sub(i32::from(field.logical_minimum) as i64)?;

            //scale to AXIS_RESOLUTION
            new_value = (new_value * AXIS_RESOLUTION as i64 * 1000) / (field.field_range()? as i64 * 1000);

            Some(new_value.clamp(0, AXIS_RESOLUTION as i64) as u64)
        }
    }

    // handles x_axis inputs
    fn x_axis_handler(&mut self, field: VariableField, report: &[u8]) {
        if let Some(x_value) = Self::resolve_axis(self.current_state.current_x, field, report) {
            if self.current_state.current_x != x_value {
                self.current_state.current_x = x_value;
                self.state_changed = true;
            }
        }
    }

    // handles y_axis inputs
    fn y_axis_handler(&mut self, field: VariableField, report: &[u8]) {
        if let Some(y_value) = Self::resolve_axis(self.current_state.current_y, field, report) {
            if self.current_state.current_y != y_value {
                self.current_state.current_y = y_value;
                self.state_changed = true;
            }
        }
    }

    // handles z_axis inputs
    fn z_axis_handler(&mut self, field: VariableField, report: &[u8]) {
        if let Some(z_value) = Self::resolve_axis(self.current_state.current_z, field, report) {
            if self.current_state.current_z != z_value {
                self.current_state.current_z = z_value;
                self.state_changed = true;
            }
        }
    }

    // handles button inputs
    fn button_handler(&mut self, field: VariableField, report: &[u8]) {
        let shift = match field.usage.into() {
            x @ BUTTON_MIN..=BUTTON_MAX => x - BUTTON_MIN,
            x @ DIGITIZER_SWITCH_MIN..=DIGITIZER_SWITCH_MAX => x - DIGITIZER_SWITCH_MIN,
            _ => return,
        };

        if let Some(button_value) = field.field_value(report) {
            let button_value = button_value as u32;

            if shift > u32::BITS {
                return;
            }
            let button_value = button_value << shift;

            let new_buttons = self.current_state.active_buttons
        & !(1 << shift)  // zero the relevant bit in the button state field.
        | button_value; // or in the current button state into that bit position.

            if new_buttons != self.current_state.active_buttons {
                self.current_state.active_buttons = new_buttons;
                self.state_changed = true;
            }
        }
    }

    fn reset_state(&mut self) {
        self.current_state = Default::default();
        // initialize pointer to center of screen
        self.current_state.current_x = CENTER;
        self.current_state.current_y = CENTER;
        self.state_changed = false;
    }
}

impl HidReportReceiver for PointerHidHandler {
    fn initialize(&mut self, controller: efi::Handle, hid_io: &dyn HidIo) -> Result<(), efi::Status> {
        let descriptor = hid_io.get_report_descriptor()?;
        self.process_descriptor(descriptor)?;

        PointerContext::install(controller, self)?;

        self.controller = Some(controller);

        Ok(())
    }
    fn receive_report(&mut self, report: &[u8], _hid_io: &dyn HidIo) {
        let old_tpl = static_boot_services().raise_tpl(Tpl::NOTIFY);

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
                    //break 'report_processing;
                }

                // hand the report data to the handler for each relevant field for field-specific processing.
                for field in report_data.relevant_fields {
                    (field.report_handler)(self, field.field, report);
                }
            }
        }

        static_boot_services().restore_tpl(old_tpl);
    }
}

impl Drop for PointerHidHandler {
    fn drop(&mut self) {
        if let Some(controller) = self.controller {
            let status = PointerContext::uninstall(self.agent, controller);
            if status.is_err() {
                debugln!(DEBUG_ERROR, "{:?}: Failed to uninstall absolute_pointer: {:?}", function!(), status);
            }
        }
    }
}
