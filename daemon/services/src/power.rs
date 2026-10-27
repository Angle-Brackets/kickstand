// SPDX-License-Identifier: GPL-3.0-only
//! Warm reboot to a payload (hekate), the only kind of reboot the daemon does: on an RCM-injected console
//! a cold reboot or power-off loses the exploit until someone reinjects by hand.
//!
//! Atmosphère's `bpc` mitm only redirects reboots made by am, omm, fatal and hbl, so a sysmodule calling
//! `bpc` RebootSystem would really cold reboot. Instead, like `reboot_to_payload` (Atmosphère,
//! troposphere): hand the payload to ams_mitm with `bpc:ams` SetRebootPayload (65001), then ask exosphere
//! directly with `spl` SetConfig(ExosphereNeedsReboot = 65001, ToPayload = 2), which on Erista jumps to
//! that payload (exosphere `secmon_smc_info.cpp`). Erista only: Mariko has no reboot-to-payload.

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

/// Makes `payload` the one Atmosphère reboots into (this also applies to the power menu's Restart).
pub fn set_reboot_payload(payload: &[u8; MAX_PAYLOAD_SIZE]) -> Result<()> {
    service::new_named_port_object::<AmsBpcService>()?.set_reboot_payload(sf::Buffer::from_array(payload))
}

/// Reboots to the payload set with [`set_reboot_payload`] (or Atmosphère's `reboot_payload.bin`). Only
/// returns on failure.
pub fn reboot_to_payload() -> Result<()> {
    let spl = service::new_service_object::<SplService>()?;
    spl.set_config(SetConfigArgs { item: EXOSPHERE_NEEDS_REBOOT, pad: 0, value: USER_REBOOT_TYPE_TO_PAYLOAD })
}
