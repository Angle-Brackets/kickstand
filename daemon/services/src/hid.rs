// SPDX-License-Identifier: GPL-3.0-only
//! `hid:dbg` HDLS: a virtual controller the system treats like a real one, so the daemon can press buttons
//! (the lock screen, menus, the game) with nobody at the console. Ids and layouts from libnx
//! `source/services/hiddbg.c` and `include/switch/services/hiddbg.h`, 13.0+ forms only (the session id
//! argument and the 12.0+ state layout).

use alloc::boxed::Box;

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;
use nx::svc;

use crate::power::{PAGE_SIZE, Page};

/// `HiddbgHdlsDeviceInfo`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct DeviceInfo {
    pub device_type: u8,
    pub npad_interface_type: u8,
    pub pad: [u8; 2],
    pub body_color: u32,
    pub buttons_color: u32,
    pub left_grip_color: u32,
    pub right_grip_color: u32,
}

/// `HiddbgHdlsState` (12.0+). The six-axis vectors are three f32s each, left zero.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct State {
    pub battery_level: u32,
    /// BIT(0) powered, BIT(1) charging.
    pub flags: u32,
    /// `HidNpadButton` bits, plus HOME (18) and Capture (19).
    pub buttons: u64,
    pub stick_l: [i32; 2],
    pub stick_r: [i32; 2],
    pub accel: [u32; 3],
    pub angle: [u32; 3],
    pub attribute: u32,
    pub indicator: u8,
    pub pad: [u8; 3],
}

/// The input of `hiddbgSetHdlsState` on 12.0+.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct SetStateArgs {
    pub handle: u64,
    pub state: State,
}

#[nx_derive::ipc_trait]
pub trait HidDbgInterface {
    /// `hiddbgAttachHdlsWorkBuffer`: returns the HDLS session id.
    #[ipc_rid(324)]
    fn attach_hdls_work_buffer(&self, size: u64, tmem: sf::CopyHandle) -> u64;
    #[ipc_rid(325)]
    fn release_hdls_work_buffer(&self, session_id: u64);
    /// Returns the device's handle.
    #[ipc_rid(330)]
    fn attach_hdls_virtual_device(&self, info: DeviceInfo) -> u64;
    #[ipc_rid(331)]
    fn detach_hdls_virtual_device(&self, handle: u64);
    #[ipc_rid(332)]
    fn set_hdls_state(&self, args: SetStateArgs);
}

nx::ipc_client_define_client_default!(HidDbgService);
impl IHidDbgInterfaceClient for HidDbgService {}

impl service::IService for HidDbgService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("hid:dbg")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// `HidDeviceType_FullKey3`: a Pro Controller.
const DEVICE_TYPE_PRO_CONTROLLER: u8 = 3;
const NPAD_INTERFACE_BLUETOOTH: u8 = 1;
const BATTERY_FULL: u32 = 4;
const POWERED: u32 = 1;

/// A virtual Pro Controller, attached for as long as this lives. Hold no longer than needed: while it's
/// attached, a game sees one more controller.
pub struct VirtualController {
    srv: HidDbgService,
    session_id: Option<u64>,
    handle: Option<u64>,
    tmem: svc::Handle,
    /// hid's work buffer, shared through `tmem`; freed only after the handle is closed.
    _work: Box<Page>,
}

impl VirtualController {
    pub fn attach() -> Result<Self> {
        let srv = service::new_service_object::<HidDbgService>()?;
        let mut work = Box::new(Page([0; PAGE_SIZE]));
        let perm = svc::MemoryPermission::Read() | svc::MemoryPermission::Write();
        let tmem = svc::create_transfer_memory(work.0.as_mut_ptr(), PAGE_SIZE, perm)?;
        let mut this = VirtualController { srv, session_id: None, handle: None, tmem, _work: work };
        // From here `Drop` undoes whatever succeeded.
        this.session_id = Some(this.srv.attach_hdls_work_buffer(PAGE_SIZE as u64, sf::CopyHandle::from(tmem))?);
        let info = DeviceInfo {
            device_type: DEVICE_TYPE_PRO_CONTROLLER,
            npad_interface_type: NPAD_INTERFACE_BLUETOOTH,
            body_color: 0xFF1C_1715,
            buttons_color: 0xFFEA_F0F2,
            left_grip_color: 0xFF54_45FF,
            right_grip_color: 0xFF54_45FF,
            ..Default::default()
        };
        this.handle = Some(this.srv.attach_hdls_virtual_device(info)?);
        Ok(this)
    }

    /// Sets which buttons are held (`HidNpadButton` bits), sticks centred.
    pub fn set_buttons(&mut self, buttons: u64) -> Result<()> {
        let state = State { battery_level: BATTERY_FULL, flags: POWERED, buttons, ..Default::default() };
        let Some(handle) = self.handle else { unreachable!("attach returned without a device") };
        self.srv.set_hdls_state(SetStateArgs { handle, state })
    }
}

impl Drop for VirtualController {
    fn drop(&mut self) {
        if let Some(handle) = self.handle {
            let _ = self.srv.detach_hdls_virtual_device(handle);
        }
        if let Some(id) = self.session_id {
            let _ = self.srv.release_hdls_work_buffer(id);
        }
        let _ = svc::close_handle(self.tmem);
    }
}

/// Gives hid and the foreground (home menu, lock screen, game) time to notice a new controller before it
/// sends input; presses that arrive sooner can be dropped.
const SETTLE_NS: i64 = 1_000_000_000;
/// Between a release and the next press.
const GAP_NS: i64 = 150_000_000;

/// Attaches a virtual controller, presses `buttons` together `times` times (holding each `hold_ms`), and
/// detaches it again.
pub fn press(buttons: u64, times: u32, hold_ms: u32) -> Result<()> {
    let mut pad = VirtualController::attach()?;
    let _ = svc::sleep_thread(SETTLE_NS);
    for _ in 0..times {
        pad.set_buttons(buttons)?;
        let _ = svc::sleep_thread(i64::from(hold_ms) * 1_000_000);
        pad.set_buttons(0)?;
        let _ = svc::sleep_thread(GAP_NS);
    }
    Ok(())
}
