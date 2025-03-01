//! Creates and manages HID instances.
//!
//! The [`HidFactory`] in this module provides an implementation of the
//! [`crate::driver_binding::DriverBinding`] trait.
//!
//! When this driver is asked to manage a controller that supports HidIo,
//! the [`crate::hid_io::HidIoFactory`] associated with the factory is used
//! to create and manage a HidIo abstraction for the controller, and the
//! [`HidReceiverFactory`] is used to create a set of receivers for reports
//! from the HidIo device.
//!
//! ## Example
//! ```ignore
//! //Create a receiver factory that creates Pointer and Keyboard Handlers as receivers.
//! struct UefiReceivers {
//!    agent: efi::Handle,
//! }
//! impl HidReceiverFactory for UefiReceivers {
//!   fn new_hid_receiver_list(&self, _controller: efi::Handle) -> Result<Vec<Box<dyn HidReportReceiver>>, efi::Status> {
//!     let mut receivers: Vec<Box<dyn HidReportReceiver>> = Vec::new();
//!     receivers.push(Box::new(PointerHidHandler::new(self.agent)));
//!     receivers.push(Box::new(KeyboardHidHandler::new(self.agent)));
//!     Ok(receivers)
//!   }
//! }
//!
//! // Create new factories for HidIo, Receivers, and Hid
//! let hid_io_factory = Box::new(UefiHidIoFactory::new(image_handle));
//! let receiver_factory = Box::new(UefiReceivers { agent: image_handle });
//! let hid_factory = Box::new(HidFactory::new(hid_io_factory, receiver_factory, image_handle));
//!
//! // start up the driver binding for the hid_factory and install it with the core.
//! let hid_binding = UefiDriverBinding::new(hid_factory, image_handle);
//! hid_binding.install().expect("failed to install HID driver binding");
//! ```
//!
//! ## License
//!
//! Copyright (C) Microsoft Corporation. All rights reserved.
//!
//! SPDX-License-Identifier: BSD-2-Clause-Patent
//!
use alloc::{boxed::Box, vec::Vec};

#[cfg(test)]
use mockall::automock;

use core::ptr::NonNull;
use r_efi::efi;

use boot_services::BootServices;
use driver_binding::DriverBinding;
use mu_rust_helpers::guid::guid;
use uefi_protocol::ProtocolInterface;

use crate::hid_io::{HidIo, HidIoFactory, HidReportReceiver};

unsafe impl ProtocolInterface for HidInstance {
    const PROTOCOL_GUID: efi::Guid = HidInstance::PRIVATE_HID_CONTEXT_GUID;
}

/// This trait defines an abstraction for getting a list of receivers for HID reports.
///
/// This is used to specify to a HidFactory how it should instantiate new receivers for HID reports.
#[cfg_attr(test, automock)]
pub trait HidReceiverFactory {
    /// Generates a vector of [`crate::hid_io::HidReportReceiver`] trait objects that can handle reports from the given controller.
    fn new_hid_receiver_list(&self, controller: efi::Handle) -> Result<Vec<Box<dyn HidReportReceiver>>, efi::Status>;
}

// Context structure used to track HID instances being managed.
// This is installed as a private interface on the controller handle to associate the HID instance with the controller.
// Note: a concrete structure is used here because Box<dyn HidIo> is a fat pointer that doesn't work well for FFI.
// Wrapping it in HidInstance makes it a fixed size type: *mut HidInstance is a thin pointer that can be cast back and
// forth to c_void.
pub struct HidInstance {
    _hid_io: Box<dyn HidIo>,
}

impl HidInstance {
    // This guid {fb719b29-fda7-4359-ac68-0d46c31a7a7e} is used to associate a HidInstance with a given controller by
    // installing it as a protocol on the controller handle.
    const PRIVATE_HID_CONTEXT_GUID: efi::Guid = guid!("fb719b29-fda7-4359-ac68-0d46c31a7a7e");

    //create a new hid instance from
    fn new(hid_io: Box<dyn HidIo>) -> Self {
        HidInstance { _hid_io: hid_io }
    }
}

// Structure used to manage multiple receivers and split reports between them.
struct HidSplitter {
    receivers: Vec<Box<dyn HidReportReceiver>>,
}

impl HidReportReceiver for HidSplitter {
    //initialize is not expected, since hid splitter is generic for all
    // controllers and is fully initialized when constructed.
    fn initialize(&mut self, _controller: efi::Handle, _hid_io: &dyn HidIo) -> Result<(), efi::Status> {
        panic!("initialize not expected for HidSplitter")
    }

    //iterates over the receivers and passes the report to each one.
    fn receive_report(&mut self, report: &[u8], hid_io: &dyn HidIo) {
        for receiver in &mut self.receivers {
            receiver.receive_report(report, hid_io)
        }
    }
}

/// This structure implements provides an implementation of
/// [`crate::driver_binding::DriverBinding`] that spans private "HidInstances"
/// whenever [`HidFactory::driver_binding_start`] is called to manage a given
/// controller.
pub struct HidFactory {
    hid_io_factory: Box<dyn HidIoFactory>,
    receiver_factory: Box<dyn HidReceiverFactory>,
    agent: efi::Handle,
}

impl HidFactory {
    /// Creates a new "HidFactory".
    ///
    /// When a new HidInstance is spawned by
    /// [`HidFactory::driver_binding_start`], `hid_io_factory` is used to create a
    /// new [`crate::hid_io::HidIo`] instance to interact with the controller, and
    /// `receiver_factory` is used to create new
    /// [`crate::hid_io::HidReportReceiver`] that will process reports from the
    /// managed controller.
    ///
    /// `agent` is the handle on which the driver binding instance should be
    /// installed (expected to be the image_handle for this driver).
    pub fn new(
        hid_io_factory: Box<dyn HidIoFactory>,
        receiver_factory: Box<dyn HidReceiverFactory>,
        agent: efi::Handle,
    ) -> Self {
        HidFactory { hid_io_factory, receiver_factory, agent }
    }
}

impl DriverBinding for HidFactory {
    /// Verifies the given controller supports HidIo
    ///
    /// This is done by attempting the instantiation of a HidIo instance on it
    /// using the HidIoFactory provided at construction - if that succeeds, the
    /// controller is considered supported. Note that the actual HidIo instance
    /// constructed for the test is dropped on return.
    fn supported<T: BootServices + 'static>(
        &self,
        _boot_services: &'static T,
        controller: efi::Handle,
        _remaining_device_path: Option<NonNull<efi::protocols::device_path::Protocol>>,
    ) -> Result<bool, efi::Status> {
        match self.hid_io_factory.new_hid_io(controller, true) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Starts a new HID instance.
    ///
    /// Starts Hid support for the given controller. The HidIoFactory provided at
    /// construction is used to create a new HidIo trait object to manage the
    /// controller, and new receivers are instantiated using the
    /// HidReceiverFactory provided at construction. A private "HidInstance"
    /// structure is created and associated with the controller to own these
    /// objects as long as the instance is "running"  - i.e. until
    /// [`Self::driver_binding_stop`] is invoked for the controller.
    fn start<T: BootServices + 'static>(
        &self,
        boot_services: &'static T,
        controller: efi::Handle,
        _remaining_device_path: Option<NonNull<efi::protocols::device_path::Protocol>>,
    ) -> Result<(), efi::Status> {
        let mut hid_io = self.hid_io_factory.new_hid_io(controller, true)?;

        let mut hid_splitter = Box::new(HidSplitter { receivers: Vec::new() });

        for mut receiver in self.receiver_factory.new_hid_receiver_list(controller)? {
            if receiver.initialize(controller, hid_io.as_mut()).is_ok() {
                hid_splitter.receivers.push(receiver);
            }
        }

        if hid_splitter.receivers.is_empty() {
            return Err(efi::Status::UNSUPPORTED);
        }

        hid_io.set_report_receiver(hid_splitter)?;

        let hid_instance = Box::leak(Box::new(HidInstance::new(hid_io)));

        match boot_services.install_protocol_interface(Some(controller), hid_instance) {
            Ok(_) => Ok(()),
            Err((hid_instance, status)) => {
                drop(unsafe { Box::from_raw(hid_instance) });
                Err(status)
            }
        }
    }

    /// Stops a running HID instance.
    ///
    /// Stops Hid support for the given controller. The private "HidInstance"
    /// created by [`Self::driver_binding_start`] is reclaimed and dropped.
    fn stop<T: BootServices + 'static>(
        &self,
        boot_services: &'static T,
        controller: efi::Handle,
        _number_of_children: usize,
        _child_handle_buffer: Option<NonNull<efi::Handle>>,
    ) -> Result<(), efi::Status> {
        unsafe {
            let hid_instance = boot_services.open_protocol_unchecked(
                controller,
                &hid_io::protocol::GUID,
                self.agent,
                controller,
                efi::OPEN_PROTOCOL_GET_PROTOCOL,
            )?;
            // SAFETY: `hid_instance` is expected to be valid if handle_protocol didn't return Err
            boot_services.uninstall_protocol_interface_unchecked(
                controller,
                &hid_io::protocol::GUID,
                hid_instance,
            )?;
            drop(Box::from_raw(hid_instance));
        }
        Ok(())
    }
}
