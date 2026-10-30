// SPDX-License-Identifier: GPL-3.0-only
//! `idle:sys` (glue's IPolicyManagerSystem): the timer behind screen dimming and auto-sleep. Id from
//! switchbrew's OMM services page; libnx doesn't wrap it.

use nx::ipc::sf::sm;
use nx::result::Result;
use nx::service;

#[nx_derive::ipc_trait]
pub trait IdleSysInterface {
    /// Resets the idle counter, as user input does. Changes no settings.
    #[ipc_rid(5)]
    fn report_user_is_active(&self);
}

nx::ipc_client_define_client_default!(IdleSysService);
impl IIdleSysInterfaceClient for IdleSysService {}

impl service::IService for IdleSysService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("idle:sys")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}
