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
    fn driver_binding_supported<T: BootServices + 'static>(
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
    fn driver_binding_start<T: BootServices + 'static>(
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
    fn driver_binding_stop<T: BootServices + 'static>(
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
            boot_services.uninstall_protocol_interface_unchecked(controller, &hid_io::protocol::GUID, hid_instance)?;
            drop(Box::from_raw(hid_instance));
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use core::{ffi::c_void, mem::MaybeUninit};

    use crate::{
        hid::HidInstance,
        hid_io::{HidReportReceiver, MockHidIo, MockHidIoFactory, MockHidReportReceiver},
        test_support::{with_global_lock, MOCK_BOOT_SERVICES},
    };
    use boot_services::{c_ptr::CPtr, MockBootServices};
    use driver_binding::DriverBinding;
    use r_efi::efi;

    use super::{HidFactory, HidSplitter, MockHidReceiverFactory};

    // In this module, the usage model for boot_services is global static, and so &'static dyn UefiBootServices is used
    // throughout the API. For testing, each test will have a different set of expectations on the UefiBootServices mock
    // object, and the mock object itself expects to be "mut", which makes it hard to handle as a single global static.
    // Instead, raw pointers are used to simulate a MockUefiBootServices instance with 'static lifetime.
    // This object needs to outlive anything that uses it - once created, it will live until the end of the program.

    #[test]
    fn driver_binding_supported_should_indicate_support() {
        with_global_lock(|| {
            let boot_services = MockBootServices::new();
            unsafe { MOCK_BOOT_SERVICES.write(boot_services) };

            let mut hid_io_factory = Box::new(MockHidIoFactory::new());
            //handle 0x3 should return success.
            hid_io_factory
                .expect_new_hid_io()
                .withf_st(|controller, _| *controller == 0x3 as efi::Handle)
                .returning(|_, _| Ok(Box::new(MockHidIo::new())));
            //default for any other handles
            hid_io_factory.expect_new_hid_io().returning(|_, _| Err(efi::Status::UNSUPPORTED));

            let receiver_factory = Box::new(MockHidReceiverFactory::new());
            let agent = 0x1 as efi::Handle;
            let hid_factory = HidFactory::new(hid_io_factory, receiver_factory, agent);

            let controller = 0x2 as efi::Handle;
            assert_eq!(
                hid_factory.driver_binding_supported(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None),
                Ok(false)
            );

            let controller = 0x3 as efi::Handle;
            assert!(hid_factory
                .driver_binding_supported(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None)
                .is_ok());
            unsafe { MOCK_BOOT_SERVICES.assume_init_drop() };
        })
        .unwrap()
    }

    #[test]
    fn driver_binding_start_should_not_start_when_not_supported() {
        with_global_lock(|| {
            let boot_services = MockBootServices::new();
            unsafe { MOCK_BOOT_SERVICES.write(boot_services) };

            let mut hid_io_factory = Box::new(MockHidIoFactory::new());
            hid_io_factory
                .expect_new_hid_io()
                .withf_st(|controller, _| *controller == 0x3 as efi::Handle)
                .returning(|_, _| Ok(Box::new(MockHidIo::new())));
            hid_io_factory
                .expect_new_hid_io()
                .withf_st(|controller, _| *controller == 0x4 as efi::Handle)
                .returning(|_, _| Ok(Box::new(MockHidIo::new())));
            //default for any other handles
            hid_io_factory.expect_new_hid_io().returning(|_, _| Err(efi::Status::UNSUPPORTED));

            let mut receiver_factory = Box::new(MockHidReceiverFactory::new());
            receiver_factory
                .expect_new_hid_receiver_list()
                .withf_st(|controller| *controller == 0x4 as efi::Handle)
                .returning(|_| Ok(Vec::new()));
            receiver_factory.expect_new_hid_receiver_list().returning(|_| Err(efi::Status::UNSUPPORTED));

            let agent = 0x1 as efi::Handle;
            let hid_factory = HidFactory::new(hid_io_factory, receiver_factory, agent);

            // test: no hid_io on the handle.
            let controller = 0x02 as efi::Handle;
            assert_eq!(
                hid_factory.driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None),
                Err(efi::Status::UNSUPPORTED)
            );

            // test: hid_io present, but failed to retrieve receivers.
            let controller = 0x03 as efi::Handle;
            assert_eq!(
                hid_factory.driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None),
                Err(efi::Status::UNSUPPORTED)
            );

            // test: hid_io present, empty receiver list.
            let controller = 0x04 as efi::Handle;
            assert_eq!(
                hid_factory.driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None),
                Err(efi::Status::UNSUPPORTED)
            );

            //test: hid_io present, receiver present, receiver init indicates no support.
            let mut hid_io_factory = Box::new(MockHidIoFactory::new());
            hid_io_factory.expect_new_hid_io().returning(|_, _| Ok(Box::new(MockHidIo::new())));

            let mut receiver_factory = Box::new(MockHidReceiverFactory::new());
            receiver_factory.expect_new_hid_receiver_list().returning(|_| {
                let mut hid_receiver = MockHidReportReceiver::new();
                hid_receiver.expect_initialize().returning(|_, _| Err(efi::Status::UNSUPPORTED));
                Ok(vec![Box::new(hid_receiver)])
            });

            let hid_factory = HidFactory::new(hid_io_factory, receiver_factory, agent);
            let controller = 0x02 as efi::Handle;
            assert_eq!(
                hid_factory.driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None),
                Err(efi::Status::UNSUPPORTED)
            );
            unsafe { MOCK_BOOT_SERVICES.assume_init_drop() };
        })
        .unwrap()
    }

    #[test]
    fn driver_binding_start_should_start_when_supported() {
        with_global_lock(|| {
            let mut boot_services = MockBootServices::new();
            let agent = 0x1 as efi::Handle;

            let mut hid_io_factory = Box::new(MockHidIoFactory::new());
            hid_io_factory.expect_new_hid_io().returning(|_, _| {
                let mut hid_io = MockHidIo::new();
                hid_io.expect_set_report_receiver().returning(|_| Ok(()));
                Ok(Box::new(hid_io))
            });

            let mut receiver_factory = Box::new(MockHidReceiverFactory::new());
            receiver_factory.expect_new_hid_receiver_list().returning(|_| {
                let mut hid_receiver = MockHidReportReceiver::new();
                hid_receiver.expect_initialize().returning(|_, _| Ok(()));
                Ok(vec![Box::new(hid_receiver)])
            });

            boot_services
                .expect_install_protocol_interface()
                .returning(|handle, interface: &'static mut HidInstance| Ok((handle.unwrap(), interface.metadata())));

            unsafe { MOCK_BOOT_SERVICES.write(boot_services) };

            let hid_factory = HidFactory::new(hid_io_factory, receiver_factory, agent);
            let controller = 0x02 as efi::Handle;
            hid_factory
                .driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None)
                .unwrap();

            //test note: this will leak a HidInstance.
            unsafe { MOCK_BOOT_SERVICES.assume_init_drop() };
        })
        .unwrap()
    }

    #[test]
    fn driver_binding_start_should_stop_after_start() {
        with_global_lock(|| {
            let mut boot_services = MockBootServices::new();
            let agent = 0x1 as efi::Handle;

            let mut hid_io_factory = Box::new(MockHidIoFactory::new());
            hid_io_factory.expect_new_hid_io().returning(|_, _| {
                let mut hid_io = MockHidIo::new();
                hid_io.expect_set_report_receiver().returning(|_| Ok(()));
                Ok(Box::new(hid_io))
            });

            let mut receiver_factory = Box::new(MockHidReceiverFactory::new());
            receiver_factory.expect_new_hid_receiver_list().returning(|_| {
                let mut hid_receiver = MockHidReportReceiver::new();
                hid_receiver.expect_initialize().returning(|_, _| Ok(()));
                Ok(vec![Box::new(hid_receiver)])
            });

            static mut HID_INSTANCE: MaybeUninit<&mut HidInstance> = MaybeUninit::uninit();
            boot_services.expect_install_protocol_interface().returning(
                |handle, interface: &'static mut HidInstance| {
                    unsafe { HID_INSTANCE.write(interface) };
                    Ok((handle.unwrap(), unsafe { HID_INSTANCE.assume_init_read().metadata() }))
                },
            );

            boot_services.expect_open_protocol_unchecked().returning(|_, _, _, _, _| unsafe {
                Ok(HID_INSTANCE.assume_init_read() as *mut HidInstance as *mut c_void)
            });

            boot_services.expect_uninstall_protocol_interface_unchecked().returning(|_, _, _| Ok(()));

            unsafe { MOCK_BOOT_SERVICES.write(boot_services) };

            let hid_factory = HidFactory::new(hid_io_factory, receiver_factory, agent);
            let controller = 0x02 as efi::Handle;
            hid_factory
                .driver_binding_start(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, None)
                .unwrap();

            assert_ne!(unsafe { HID_INSTANCE.as_mut_ptr() }, core::ptr::null_mut());

            hid_factory
                .driver_binding_stop(unsafe { MOCK_BOOT_SERVICES.assume_init_mut() }, controller, 0, None)
                .unwrap();
            unsafe { MOCK_BOOT_SERVICES.assume_init_drop() };
        })
        .unwrap()
    }

    #[test]
    fn hid_splitter_should_split_things() {
        let mut mock_hid_receiver1 = MockHidReportReceiver::new();
        mock_hid_receiver1.expect_receive_report().returning(|_, _| ());
        let mut mock_hid_receiver2 = MockHidReportReceiver::new();
        mock_hid_receiver2.expect_receive_report().returning(|_, _| ());
        let receivers: Vec<Box<dyn HidReportReceiver>> =
            vec![Box::new(mock_hid_receiver1), Box::new(mock_hid_receiver2)];

        let mut hid_splitter = HidSplitter { receivers };
        let mock_hid_io = MockHidIo::new();
        hid_splitter.receive_report(&[0, 0, 0, 0], &mock_hid_io);
    }
}
