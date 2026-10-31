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

/// `HidTouchState`.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct TouchState {
    pub delta_time: u64,
    pub attributes: u32,
    pub finger_id: u32,
    pub x: u32,
    pub y: u32,
    pub diameter_x: u32,
    pub diameter_y: u32,
    pub rotation_angle: u32,
    pub reserved: u32,
}

/// The input of `hiddbgSetHdlsState` on 12.0+.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct SetStateArgs {
    pub handle: u64,
    pub state: State,
}

/// `HiddbgKeyboardAutoPilotState`: modifiers (`HidKeyboardModifier`) and a bitmap of held keys
/// (`HidKeyboardKey`, USB HID usage ids).
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct KeyboardState {
    pub modifiers: u64,
    pub keys: [u64; 4],
}

#[nx_derive::ipc_trait]
pub trait HidDbgInterface {
    /// `hiddbgSetKeyboardAutoPilotState`: the keyboard reports this state until unset.
    #[ipc_rid(31)]
    fn set_keyboard_auto_pilot_state(&self, state: KeyboardState);
    #[ipc_rid(32)]
    fn unset_keyboard_auto_pilot_state(&self);
    /// `hiddbgSetTouchScreenAutoPilotState`: the touch screen reports these touches until unset.
    #[ipc_rid(11)]
    fn set_touch_screen_auto_pilot_state(&self, states: sf::InMapAliasBuffer<TouchState>);
    #[ipc_rid(12)]
    fn unset_touch_screen_auto_pilot_state(&self);
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

/// `HidTouchAttribute_Start` and `_End`: the first and last report of a touch. The autopilot repeats its
/// state every sample, so each is shown for [`TOUCH_EDGE_NS`] and then replaced: a touch that said Start on
/// every sample never becomes a held touch, and one that vanishes without End is never a tap, which is
/// what the home menu wants (hbmenu only counts touches).
const TOUCH_START: u32 = 1 << 0;
const TOUCH_END: u32 = 1 << 1;
/// A few samples (hid samples touch about every 4 ms; apps read once a frame).
const TOUCH_EDGE_NS: i64 = 34_000_000;
/// A fingertip, in touch screen pixels.
const TOUCH_DIAMETER: u32 = 15;

/// One step of [`play`]: held buttons (`HidNpadButton` bits), stick positions ([x, y], ±0x7FFF), an
/// optional touch, for `ms`.
#[derive(Copy, Clone, Default)]
pub struct Step {
    pub buttons: u64,
    pub left: [i32; 2],
    pub right: [i32; 2],
    pub touch: Option<(u32, u32)>,
    pub ms: u32,
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
    /// Where the finger is, if it's down.
    touch: Option<(u32, u32)>,
}

impl VirtualController {
    pub fn attach() -> Result<Self> {
        let srv = service::new_service_object::<HidDbgService>()?;
        let mut work = Box::new(Page([0; PAGE_SIZE]));
        let perm = svc::MemoryPermission::Read() | svc::MemoryPermission::Write();
        let tmem = svc::create_transfer_memory(work.0.as_mut_ptr(), PAGE_SIZE, perm)?;
        let mut this = VirtualController { srv, session_id: None, handle: None, tmem, _work: work, touch: None };
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
        self.set_state(&Step { buttons, ..Default::default() })
    }

    /// Sets the buttons and sticks of `step` (not its touch).
    pub fn set_state(&mut self, step: &Step) -> Result<()> {
        let state = State {
            battery_level: BATTERY_FULL,
            flags: POWERED,
            buttons: step.buttons,
            stick_l: step.left,
            stick_r: step.right,
            ..Default::default()
        };
        let Some(handle) = self.handle else { unreachable!("attach returned without a device") };
        self.srv.set_hdls_state(SetStateArgs { handle, state })
    }

    /// Puts one finger on the touch screen at (x, y) (moving it, if it's down), or lifts it.
    pub fn set_touch(&mut self, at: Option<(u32, u32)>) -> Result<()> {
        let report = |attributes, (x, y): (u32, u32)| TouchState {
            attributes,
            x,
            y,
            diameter_x: TOUCH_DIAMETER,
            diameter_y: TOUCH_DIAMETER,
            ..Default::default()
        };
        match (self.touch, at) {
            (None, Some(at)) => {
                self.srv.set_touch_screen_auto_pilot_state(sf::Buffer::from_array(&[report(TOUCH_START, at)]))?;
                self.touch = Some(at);
                let _ = svc::sleep_thread(TOUCH_EDGE_NS);
                self.srv.set_touch_screen_auto_pilot_state(sf::Buffer::from_array(&[report(0, at)]))
            }
            (Some(_), Some(at)) => {
                self.touch = Some(at);
                self.srv.set_touch_screen_auto_pilot_state(sf::Buffer::from_array(&[report(0, at)]))
            }
            (Some(last), None) => {
                self.touch = None;
                let ended = self.srv.set_touch_screen_auto_pilot_state(sf::Buffer::from_array(&[report(TOUCH_END, last)]));
                let _ = svc::sleep_thread(TOUCH_EDGE_NS);
                // No fingers, before the real touch screen takes over again: handing over straight from a
                // touch reads as a cancelled one, so the home menu never saw taps (drags still worked).
                let none: [TouchState; 0] = [];
                let lifted = self.srv.set_touch_screen_auto_pilot_state(sf::Buffer::from_array(&none));
                let _ = svc::sleep_thread(TOUCH_EDGE_NS);
                ended.and(lifted).and(self.srv.unset_touch_screen_auto_pilot_state())
            }
            (None, None) => Ok(()),
        }
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

/// The controller [`hold`] keeps attached between requests, with what it holds and until when (uptime
/// ms). Press sequences take this lock too, so only one virtual controller is ever attached and they run
/// one at a time (a client's and the boot unlock's).
static PAD: nx::sync::Mutex<Option<Held>> = nx::sync::Mutex::new(None);

struct Held {
    pad: VirtualController,
    step: Step,
    until_ms: u64,
}

// Only used under `PAD`'s lock; the IPC sessions in it are handles the whole process may use.
unsafe impl Send for Held {}
unsafe impl Sync for Held {}

/// Holds `step`'s buttons, sticks and touch (its `ms` is ignored) on a controller kept attached until
/// `until_ms` (uptime), then [`expire_hold`] releases everything and detaches it. Called again, it
/// changes what's held and when it ends; an empty step keeps the controller attached, holding nothing, so
/// presses meanwhile skip the settling time. The first call attaches the controller and waits for it to
/// settle.
pub fn hold(step: &Step, until_ms: u64) -> Result<()> {
    let mut held = PAD.lock();
    if held.is_none() {
        let pad = VirtualController::attach()?;
        let _ = svc::sleep_thread(SETTLE_NS);
        *held = Some(Held { pad, step: Step::default(), until_ms });
    }
    let Some(held) = held.as_mut() else { unreachable!("attached above") };
    held.until_ms = until_ms;
    held.step = *step;
    apply(&mut held.pad, step)
}

/// Releases and detaches the held controller now, if there is one.
pub fn release_hold() {
    if let Some(mut held) = PAD.lock().take() {
        release(&mut held.pad);
    }
}

/// Releases and detaches the held controller once its time is up. Call it a few times a second.
pub fn expire_hold(now_ms: u64) {
    let mut held = PAD.lock();
    if held.as_ref().is_some_and(|h| now_ms >= h.until_ms)
        && let Some(mut h) = held.take()
    {
        release(&mut h.pad);
    }
}

fn apply(pad: &mut VirtualController, step: &Step) -> Result<()> {
    pad.set_state(step)?;
    if step.touch != pad.touch {
        pad.set_touch(step.touch)?;
    }
    Ok(())
}

/// Never leaves a finger down or a button held.
fn release(pad: &mut VirtualController) {
    let _ = pad.set_touch(None);
    let _ = pad.set_buttons(0);
    let _ = svc::sleep_thread(GAP_NS);
}

/// Runs `f` on the held controller (then holds its step again), or on one attached for the call.
fn with_pad(f: impl FnOnce(&mut VirtualController) -> Result<()>) -> Result<()> {
    let mut held = PAD.lock();
    if let Some(held) = held.as_mut() {
        let result = f(&mut held.pad);
        let _ = svc::sleep_thread(GAP_NS);
        return result.and(apply(&mut held.pad, &held.step));
    }
    let mut pad = VirtualController::attach()?;
    let _ = svc::sleep_thread(SETTLE_NS);
    let result = f(&mut pad);
    release(&mut pad);
    result
}

/// Holds each step's buttons, sticks and touch for its time (an empty step is a pause), then releases
/// everything. A button in consecutive steps stays held across them, e.g. R through a press of A and the
/// loading that follows. Uses the held controller if there is one, else attaches one for the call.
pub fn play(steps: &[Step]) -> Result<()> {
    with_pad(|pad| {
        for step in steps {
            apply(pad, step)?;
            let _ = svc::sleep_thread(i64::from(step.ms) * 1_000_000);
        }
        let _ = pad.set_touch(None);
        pad.set_buttons(0)
    })
}

/// Presses `buttons` together `times` times, holding each `hold_ms`, on the held controller or one
/// attached for the call.
pub fn press(buttons: u64, times: u32, hold_ms: u32) -> Result<()> {
    with_pad(|pad| {
        for _ in 0..times {
            pad.set_buttons(buttons)?;
            let _ = svc::sleep_thread(i64::from(hold_ms) * 1_000_000);
            pad.set_buttons(0)?;
            let _ = svc::sleep_thread(GAP_NS);
        }
        Ok(())
    })
}

/// Like [`play`], but on the held controller (attached and settled first if there's none), and then it
/// keeps holding the last step until `lease_end()` (uptime ms, asked once the steps have played) instead
/// of letting go: the next call carries on with nothing released in between.
pub fn play_held(steps: &[Step], lease_end: impl FnOnce() -> u64) -> Result<()> {
    let mut held = PAD.lock();
    if held.is_none() {
        let pad = VirtualController::attach()?;
        let _ = svc::sleep_thread(SETTLE_NS);
        // `expire_hold` waits on this lock, so it can't let go while the steps play.
        *held = Some(Held { pad, step: Step::default(), until_ms: u64::MAX });
    }
    let Some(held) = held.as_mut() else { unreachable!("attached above") };
    for step in steps {
        apply(&mut held.pad, step)?;
        let _ = svc::sleep_thread(i64::from(step.ms) * 1_000_000);
    }
    held.step = steps.last().copied().unwrap_or_default();
    held.until_ms = lease_end();
    Ok(())
}

/// How long a key stays down, and the gap after it: long enough for an app reading once a frame.
const KEY_DOWN_NS: i64 = 40_000_000;
const KEY_UP_NS: i64 = 30_000_000;
/// `HidKeyboardModifier_Shift`.
const SHIFT: u64 = 1 << 1;

/// The key (a USB HID usage id) and whether Shift is needed for `c` on a US layout.
pub fn key_for(c: char) -> Option<(u8, bool)> {
    const SHIFTED_DIGITS: &str = "!@#$%^&*()";
    Some(match c {
        'a'..='z' => (c as u8 - b'a' + 4, false),
        'A'..='Z' => (c as u8 - b'A' + 4, true),
        '1'..='9' => (c as u8 - b'1' + 30, false),
        '0' => (39, false),
        '\n' => (40, false),
        '\t' => (43, false),
        ' ' => (44, false),
        '-' => (45, false),
        '_' => (45, true),
        '=' => (46, false),
        '+' => (46, true),
        '[' => (47, false),
        '{' => (47, true),
        ']' => (48, false),
        '}' => (48, true),
        '\\' => (49, false),
        '|' => (49, true),
        ';' => (51, false),
        ':' => (51, true),
        '\'' => (52, false),
        '"' => (52, true),
        '`' => (53, false),
        '~' => (53, true),
        ',' => (54, false),
        '<' => (54, true),
        '.' => (55, false),
        '>' => (55, true),
        '/' => (56, false),
        '?' => (56, true),
        _ => {
            let i = SHIFTED_DIGITS.find(c)?;
            // ! is Shift+1 ... ) is Shift+0.
            (if i == 9 { 39 } else { 30 + i as u8 }, true)
        }
    })
}

/// Types `text` on the keyboard autopilot, a key at a time, then gives the keyboard back. Characters
/// [`key_for`] doesn't know must be refused before this.
pub fn type_text(text: &str) -> Result<()> {
    let srv = service::new_service_object::<HidDbgService>()?;
    let result = (|| {
        for c in text.chars() {
            let Some((key, shift)) = key_for(c) else { continue };
            let mut down = KeyboardState { modifiers: if shift { SHIFT } else { 0 }, ..Default::default() };
            down.keys[usize::from(key / 64)] |= 1 << (key % 64);
            srv.set_keyboard_auto_pilot_state(down)?;
            let _ = svc::sleep_thread(KEY_DOWN_NS);
            srv.set_keyboard_auto_pilot_state(KeyboardState::default())?;
            let _ = svc::sleep_thread(KEY_UP_NS);
        }
        Ok(())
    })();
    let _ = srv.unset_keyboard_auto_pilot_state();
    result
}
