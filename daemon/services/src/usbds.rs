// SPDX-License-Identifier: GPL-3.0-only
//! `usb:ds`: the console as a USB device on its USB-C port, with one vendor-specific interface of a bulk
//! IN and a bulk OUT endpoint. Command ids (the 11.0+ ones only), descriptor layouts and the setup order
//! from libnx `source/services/usbds.c` and `source/runtime/devices/usb_comms.c` (ISC).

use alloc::alloc::{Layout, alloc_zeroed, dealloc};

use nx::ipc::sf::{self, sm};
use nx::result::{Result, ResultCode};
use nx::service;
use nx::svc::{self, Handle};
use nx::version::{self, Version};

#[nx_derive::ipc_trait]
pub trait DsRoot {
    /// `usbDsInitialize` on 11.0+: the root service hands out the real one.
    #[ipc_rid(0)]
    fn open_ds_service(&self) -> DsService;
}

#[nx_derive::ipc_trait]
#[default_client]
pub trait DsService {
    #[ipc_rid(0)]
    fn bind_device(&self, complex_id: u32, process: sf::CopyHandle);
    #[ipc_rid(1)]
    fn register_interface(&self, interface_number: u8) -> DsInterface;
    #[ipc_rid(2)]
    fn get_state_change_event(&self) -> sf::CopyHandle;
    #[ipc_rid(3)]
    fn get_state(&self) -> u32;
    #[ipc_rid(4)]
    fn clear_device_data(&self);
    #[ipc_rid(5)]
    fn add_usb_string_descriptor(&self, descriptor: sf::InMapAliasBuffer<'_, u8>) -> u8;
    #[ipc_rid(7)]
    fn set_usb_device_descriptor(&self, speed: u32, descriptor: sf::InMapAliasBuffer<'_, u8>);
    #[ipc_rid(8)]
    fn set_binary_object_store(&self, bos: sf::InMapAliasBuffer<'_, u8>);
    #[ipc_rid(9)]
    fn enable(&self);
    #[ipc_rid(10)]
    fn disable(&self);
    #[ipc_rid(11)]
    fn get_speed(&self) -> u32;
}

#[nx_derive::ipc_trait]
#[default_client]
pub trait DsInterface {
    #[ipc_rid(0)]
    fn register_endpoint(&self, address: u8) -> DsEndpoint;
    #[ipc_rid(10)]
    fn append_configuration_data(&self, speed: u32, data: sf::InMapAliasBuffer<'_, u8>);
}

#[nx_derive::ipc_trait]
#[default_client]
pub trait DsEndpoint {
    #[ipc_rid(0)]
    fn post_buffer_async(&self, size: u32, pad: u32, address: u64) -> u32;
    #[ipc_rid(1)]
    fn cancel(&self);
    #[ipc_rid(2)]
    fn get_completion_event(&self) -> sf::CopyHandle;
    #[ipc_rid(3)]
    fn get_report_data(&self) -> ReportData;
}

/// `UsbDsReportEntry`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct ReportEntry {
    pub id: u32,
    pub requested_size: u32,
    pub transferred_size: u32,
    pub urb_status: u32,
}

/// `UsbDsReportData`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct ReportData {
    pub report: [ReportEntry; 8],
    pub report_count: u32,
}

nx::ipc_client_define_client_default!(RootService);
impl IDsRootClient for RootService {}

impl service::IService for RootService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("usb:ds")
    }

    fn as_domain() -> bool {
        true
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// `UsbState_Configured`: the host picked our configuration, so the endpoints work.
pub const STATE_CONFIGURED: u32 = 5;
/// `UsbComplexId_Default`.
const COMPLEX_ID: u32 = 2;
/// `UsbDeviceSpeed`.
const SPEED_FULL: u32 = 2;
const SPEED_HIGH: u32 = 3;
const SPEED_SUPER: u32 = 4;
/// Transfer buffers must start on a page.
const PAGE: usize = 0x1000;

/// What the PC finds: ids, strings and the interface's class, subclass and protocol.
pub struct Identity<'a> {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: &'a str,
    pub product: &'a str,
    pub serial: &'a str,
    pub class: (u8, u8, u8),
}

/// A direction on the interface's two bulk endpoints.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Device to host.
    In,
    /// Host to device.
    Out,
}

/// The device, enabled: the PC sees it until this is dropped. Fields drop in order, endpoints before the
/// interface before the services they came from.
pub struct Device {
    endpoint_in: DsEndpoint,
    endpoint_out: DsEndpoint,
    completion_in: Event,
    completion_out: Event,
    _interface: DsInterface,
    state_change: Event,
    ds: DsService,
    _root: RootService,
}

/// A kernel event handle, closed on drop.
struct Event(Handle);

impl Drop for Event {
    fn drop(&mut self) {
        let _ = svc::close_handle(self.0);
    }
}

impl Device {
    /// Binds `usb:ds` and presents `identity`. Fails if another program has the port (haze, DBI, Goldleaf),
    /// or before firmware 11.0 (whose command ids differ).
    pub fn open(identity: &Identity) -> Result<Device> {
        if version::get_version() < Version::new(11, 0, 0) {
            return ResultCode::new_err(LIBNX_INCOMPATIBLE_SYSTEM_VERSION);
        }
        let root = service::new_service_object::<RootService>()?;
        let ds = root.open_ds_service()?;
        ds.bind_device(COMPLEX_ID, sf::CopyHandle::from(svc::CURRENT_PROCESS_PSEUDO_HANDLE))?;
        let state_change = Event(ds.get_state_change_event()?.handle);
        ds.clear_device_data()?;

        ds.add_usb_string_descriptor(sf::Buffer::from_array(&string_descriptor(&[0x0409])))?;
        let manufacturer = ds.add_usb_string_descriptor(sf::Buffer::from_array(&text_descriptor(identity.manufacturer)))?;
        let product = ds.add_usb_string_descriptor(sf::Buffer::from_array(&text_descriptor(identity.product)))?;
        let serial = ds.add_usb_string_descriptor(sf::Buffer::from_array(&text_descriptor(identity.serial)))?;
        let [vid_lo, vid_hi] = identity.vendor_id.to_le_bytes();
        let [pid_lo, pid_hi] = identity.product_id.to_le_bytes();
        let device = |bcd_usb: u16, max_packet0: u8| -> [u8; 18] {
            let [usb_lo, usb_hi] = bcd_usb.to_le_bytes();
            [18, 1, usb_lo, usb_hi, 0, 0, 0, max_packet0, vid_lo, vid_hi, pid_lo, pid_hi, 0x00, 0x01, manufacturer, product, serial, 1]
        };
        ds.set_usb_device_descriptor(SPEED_FULL, sf::Buffer::from_array(&device(0x0110, 0x40)))?;
        ds.set_usb_device_descriptor(SPEED_HIGH, sf::Buffer::from_array(&device(0x0200, 0x40)))?;
        // SuperSpeed's control endpoint packet size is 2^9.
        ds.set_usb_device_descriptor(SPEED_SUPER, sf::Buffer::from_array(&device(0x0300, 0x09)))?;
        ds.set_binary_object_store(sf::Buffer::from_array(&BOS))?;

        // The first free interface number (libnx tries 0 to 3).
        let mut registered = None;
        for number in 0..4u8 {
            if let Ok(interface) = ds.register_interface(number) {
                registered = Some((number, interface));
                break;
            }
        }
        let Some((number, interface)) = registered else {
            return ResultCode::new_err(LIBNX_OUT_OF_MEMORY);
        };
        let (class, subclass, protocol) = identity.class;
        let interface_descriptor = [9, 4, number, 0, 2, class, subclass, protocol, 0];
        let (address_in, address_out) = (0x80 | (number + 1), number + 1);
        let endpoint = |address: u8, max_packet: u16| -> [u8; 7] {
            let [lo, hi] = max_packet.to_le_bytes();
            [7, 5, address, 2, lo, hi, 0]
        };
        // Bursts of 15 packets on SuperSpeed.
        let companion = [6, 0x30, 0x0F, 0, 0, 0];
        for (speed, max_packet) in [(SPEED_FULL, 0x40), (SPEED_HIGH, 0x200), (SPEED_SUPER, 0x400)] {
            interface.append_configuration_data(speed, sf::Buffer::from_array(&interface_descriptor))?;
            interface.append_configuration_data(speed, sf::Buffer::from_array(&endpoint(address_in, max_packet)))?;
            if speed == SPEED_SUPER {
                interface.append_configuration_data(speed, sf::Buffer::from_array(&companion))?;
            }
            interface.append_configuration_data(speed, sf::Buffer::from_array(&endpoint(address_out, max_packet)))?;
            if speed == SPEED_SUPER {
                interface.append_configuration_data(speed, sf::Buffer::from_array(&companion))?;
            }
        }
        let endpoint_in = interface.register_endpoint(address_in)?;
        let endpoint_out = interface.register_endpoint(address_out)?;
        let completion_in = Event(endpoint_in.get_completion_event()?.handle);
        let completion_out = Event(endpoint_out.get_completion_event()?.handle);
        ds.enable()?;
        Ok(Device { endpoint_in, endpoint_out, completion_in, completion_out, _interface: interface, state_change, ds, _root: root })
    }

    /// `UsbState`: [`STATE_CONFIGURED`] once the host has set the device up.
    pub fn state(&self) -> Result<u32> {
        self.ds.get_state()
    }

    /// Signalled when [`Device::state`] changes; clear it with [`Device::clear_state_change`].
    pub fn state_change(&self) -> Handle {
        self.state_change.0
    }

    pub fn clear_state_change(&self) {
        let _ = svc::reset_signal(self.state_change.0);
    }

    /// Signalled when a transfer posted on `dir` ends.
    pub fn completion(&self, dir: Dir) -> Handle {
        match dir {
            Dir::In => self.completion_in.0,
            Dir::Out => self.completion_out.0,
        }
    }

    fn endpoint(&self, dir: Dir) -> &DsEndpoint {
        match dir {
            Dir::In => &self.endpoint_in,
            Dir::Out => &self.endpoint_out,
        }
    }

    /// Starts a transfer of `buf[..len]` (sent for [`Dir::In`], filled for [`Dir::Out`]); returns its id for
    /// [`Device::finish`] once [`Device::completion`] is signalled. `buf` must stay untouched until then.
    pub fn post(&self, dir: Dir, buf: &mut PageBuf, len: usize) -> Result<u32> {
        let len = len.min(buf.len);
        nx::arm::cache_flush(buf.ptr, len);
        self.endpoint(dir).post_buffer_async(len as u32, 0, buf.ptr as u64)
    }

    /// Bytes a finished transfer moved (`usbDsParseReportData`).
    pub fn finish(&self, dir: Dir, urb: u32) -> Result<usize> {
        let _ = svc::reset_signal(self.completion(dir));
        let data = self.endpoint(dir).get_report_data()?;
        let count = (data.report_count as usize).min(data.report.len());
        let Some(entry) = data.report[..count].iter().find(|e| e.id == urb) else {
            return ResultCode::new_err(LIBNX_NOT_FOUND);
        };
        match entry.urb_status {
            3 => Ok((entry.transferred_size as usize).min(entry.requested_size as usize)),
            4 => ResultCode::new_err(0x828c),
            5 => ResultCode::new_err(0x748c),
            _ => ResultCode::new_err(0x108c),
        }
    }

    /// Cancels `dir`'s transfer, waiting (a little) for it to end so its buffer is free again.
    pub fn cancel(&self, dir: Dir) {
        let _ = self.endpoint(dir).cancel();
        let _ = svc::wait_synchronization_one(self.completion(dir), 100_000_000);
        let _ = svc::reset_signal(self.completion(dir));
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.endpoint_in.cancel();
        let _ = self.endpoint_out.cancel();
        let _ = self.ds.disable();
    }
}

/// Page-aligned heap memory for transfers.
pub struct PageBuf {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the buffer is plain memory owned by this value.
unsafe impl Send for PageBuf {}

impl PageBuf {
    /// `None` without the memory.
    pub fn new(len: usize) -> Option<PageBuf> {
        let layout = Layout::from_size_align(len.next_multiple_of(PAGE), PAGE).ok()?;
        // SAFETY: the layout's size isn't zero for any `len` > 0, and a zero one is refused below.
        let ptr = if layout.size() == 0 { core::ptr::null_mut() } else { unsafe { alloc_zeroed(layout) } };
        (!ptr.is_null()).then_some(PageBuf { ptr, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` holds `len` initialised bytes for as long as `self`.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`, borrowed mutably through `self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for PageBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr, Layout::from_size_align_unchecked(self.len.next_multiple_of(PAGE), PAGE)) }
    }
}

/// `MAKERESULT(Module_Libnx, ...)` codes, for failures that aren't the service's.
const LIBNX_OUT_OF_MEMORY: u32 = 0x159 | (2 << 9);
const LIBNX_NOT_FOUND: u32 = 0x159 | (13 << 9);
const LIBNX_INCOMPATIBLE_SYSTEM_VERSION: u32 = 0x159 | (100 << 9);

/// The Binary Object Store libnx sends: USB 2.0 extension (LPM) and SuperSpeed capabilities.
const BOS: [u8; 0x16] = [
    0x05, 0x0F, 0x16, 0x00, 0x02, //
    0x07, 0x10, 0x02, 0x02, 0x00, 0x00, 0x00, //
    0x0A, 0x10, 0x03, 0x00, 0x0E, 0x00, 0x03, 0x00, 0x00, 0x00,
];

/// `struct usb_string_descriptor`: always its full 0x82 bytes, `bLength` saying how much counts.
fn string_descriptor(units: &[u16]) -> [u8; 0x82] {
    let mut d = [0u8; 0x82];
    let units = &units[..units.len().min(0x40)];
    d[0] = (2 + 2 * units.len()) as u8;
    d[1] = 3;
    for (i, unit) in units.iter().enumerate() {
        d[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    d
}

fn text_descriptor(text: &str) -> [u8; 0x82] {
    let mut units = [0u16; 0x3F];
    let mut n = 0;
    for unit in text.encode_utf16().take(units.len()) {
        units[n] = unit;
        n += 1;
    }
    string_descriptor(&units[..n])
}
