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
//! Don't call `spl` SetConfig(ExosphereNeedsReboot, ToPayload) directly: exosphere then reboots into
//! whatever is in IRAM, which without ams_mitm's copy is garbage. That hung the console (2026-10-26). A
//! sysmodule would need ams_mitm's IRAM copy itself (the Atmosphère IramCopy smc, 0xF0000201).

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
