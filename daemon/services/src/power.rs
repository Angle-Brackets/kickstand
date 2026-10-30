// SPDX-License-Identifier: GPL-3.0-only
//! Warm reboot to a payload (hekate), the only kind of reboot the daemon does: on an RCM-injected console
//! a cold reboot or power-off loses the exploit until someone reinjects by hand.
//!
//! Atmosphère's `bpc` mitm redirects the reboots made by am, omm, fatal and hbl, which covers the NRO
//! (it runs under hbl). Its RebootSystem clears IRAM, copies the payload to 0x40010000 and only then asks
//! exosphere to reboot into it (ams_mitm `bpc_ams_power_utils.cpp`, DoRebootToPayload). So: hand the
//! payload to ams_mitm with `bpc:ams` SetRebootPayload (65001), then call `bpc` RebootSystem (1) from
//! the NRO. Erista only: Mariko has no reboot-to-payload.
//!
//! The sysmodule isn't redirected, so it does ams_mitm's part itself ([`reboot_to_payload_direct`]): fill
//! IRAM with 0xCC and copy the payload to 0x40010000, a page at a time, with Atmosphère's IramCopy smc
//! (0xF0000201, exosphere `secmon_smc_memory_access.cpp`), then ask exosphere to reboot to it through `spl`
//! SetConfig(ExosphereNeedsReboot = 65001, ToPayload = 2). The IramCopy function id has bit 9 set, so the
//! kernel turns our pointer in x1 into a physical address (Mesosphere `CallSecureMonitorFromUser`); the
//! page must be ordinary read-write memory. Needs svc 0x7F (CallSecureMonitor) in the npdm, which the NRO
//! (running as hbl's host title) doesn't have: calling it there would crash.
//!
//! Never skip the IRAM copy: SetConfig alone reboots into whatever IRAM holds, which hung the console
//! (2026-10-26). Never issue Nintendo's own SMCs with svc 0x7F either: exosphere only takes those from spl
//! on core 3 and panics otherwise. Atmosphère's 0xF000xxxx ones are allowed from anywhere.

use core::ffi::CStr;

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;

/// Largest payload ams_mitm accepts; it aborts (taking ams_mitm down) on anything bigger.
pub const MAX_PAYLOAD_SIZE: usize = 0x24000;

#[nx_derive::ipc_trait]
pub trait AmsBpcInterface {
    #[ipc_rid(65001)]
    fn set_reboot_payload(&self, payload: sf::InMapAliasBuffer<u8>);
}

nx::ipc_client_define_client_default!(AmsBpcService);
impl IAmsBpcInterfaceClient for AmsBpcService {}

impl service::INamedPort for AmsBpcService {
    fn get_name() -> &'static CStr {
        c"bpc:ams"
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

#[nx_derive::ipc_trait]
pub trait BpcInterface {
    #[ipc_rid(1)]
    fn reboot_system(&self);
}

nx::ipc_client_define_client_default!(BpcService);
impl IBpcInterfaceClient for BpcService {}

impl service::IService for BpcService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("bpc")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The input of `splSetConfig` (libnx `spl.c`): `{ u32 config_item; u64 value; }`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct SetConfigArgs {
    pub item: u32,
    pub pad: u32,
    pub value: u64,
}

#[nx_derive::ipc_trait]
pub trait SplInterface {
    #[ipc_rid(5)]
    fn set_config(&self, args: SetConfigArgs);
}

nx::ipc_client_define_client_default!(SplService);
impl ISplInterfaceClient for SplService {}

impl service::IService for SplService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("spl:")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

const EXOSPHERE_NEEDS_REBOOT: u32 = 65001;
const USER_REBOOT_TYPE_TO_PAYLOAD: u64 = 2;

pub const IRAM_BASE: usize = 0x4000_0000;
pub const IRAM_SIZE: usize = 0x4_0000;
/// Where exosphere's reboot stub jumps to.
pub const IRAM_PAYLOAD_OFFSET: usize = 0x1_0000;
pub const PAGE_SIZE: usize = 0x1000;
const SMC_ATMOSPHERE_IRAM_COPY: u64 = 0xF000_0201;

/// One page of ordinary memory, the unit IramCopy works in.
#[repr(C, align(4096))]
pub struct Page(pub [u8; PAGE_SIZE]);

/// Copies one page between `page` and IRAM at `offset` (page aligned). Errors are exosphere's SmcResult.
fn iram_copy(page: &mut Page, offset: usize, to_iram: bool) -> core::result::Result<(), u64> {
    if !offset.is_multiple_of(PAGE_SIZE) || offset >= IRAM_SIZE {
        return Err(u64::MAX);
    }
    let args = [
        SMC_ATMOSPHERE_IRAM_COPY,
        page.0.as_mut_ptr() as u64,
        (IRAM_BASE + offset) as u64,
        PAGE_SIZE as u64,
        to_iram as u64,
        0,
        0,
        0,
    ];
    match nx::svc::call_secure_monitor(args)[0] {
        0 => Ok(()),
        err => Err(err),
    }
}

/// Reads the IRAM page at `offset`. Sysmodule only (see the module docs).
pub fn iram_read(offset: usize, page: &mut Page) -> core::result::Result<(), u64> {
    iram_copy(page, offset, false)
}

/// Does what ams_mitm does for a redirected reboot, for a process it doesn't redirect: clears IRAM, copies
/// `payload` to [`IRAM_PAYLOAD_OFFSET`] and reboots into it. Only returns on failure, with the step that
/// failed. Sysmodule only.
pub fn reboot_to_payload_direct(payload: &[u8; MAX_PAYLOAD_SIZE]) -> core::result::Result<(), (&'static str, u64)> {
    let mut page = alloc::boxed::Box::new(Page([0xCC; PAGE_SIZE]));
    for offset in (0..IRAM_SIZE).step_by(PAGE_SIZE) {
        iram_copy(&mut page, offset, true).map_err(|e| ("clearing IRAM", e))?;
    }
    for (i, chunk) in payload.chunks(PAGE_SIZE).enumerate() {
        page.0.fill(0xCC);
        page.0[..chunk.len()].copy_from_slice(chunk);
        iram_copy(&mut page, IRAM_PAYLOAD_OFFSET + i * PAGE_SIZE, true).map_err(|e| ("copying the payload to IRAM", e))?;
    }
    let spl = service::new_service_object::<SplService>().map_err(|rc| ("opening spl:", rc.get_value() as u64))?;
    spl.set_config(SetConfigArgs { item: EXOSPHERE_NEEDS_REBOOT, pad: 0, value: USER_REBOOT_TYPE_TO_PAYLOAD })
        .map_err(|rc| ("spl SetConfig(ExosphereNeedsReboot)", rc.get_value() as u64))
}

/// Makes `payload` the one Atmosphère reboots into (this also applies to the power menu's Restart).
pub fn set_reboot_payload(payload: &[u8; MAX_PAYLOAD_SIZE]) -> Result<()> {
    service::new_named_port_object::<AmsBpcService>()?.set_reboot_payload(sf::Buffer::from_array(payload))
}

/// Reboots to the payload set with [`set_reboot_payload`] (or Atmosphère's `reboot_payload.bin`). Only
/// returns on failure. Call this only from the NRO: from any process ams_mitm doesn't redirect (the
/// sysmodule included) it is a cold reboot.
pub fn reboot_to_payload() -> Result<()> {
    service::new_service_object::<BpcService>()?.reboot_system()
}
