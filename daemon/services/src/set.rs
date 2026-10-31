// SPDX-License-Identifier: GPL-3.0-only
//! `set:sys`: the console's nickname (System Settings > System > Console Nickname), for discovery
//! answers. Command id and the 0x80-byte buffer from libnx `setsysGetDeviceNickname` (ISC).

use alloc::string::String;

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;

#[nx_derive::ipc_trait]
pub trait SetSysInterface {
    #[ipc_rid(77)]
    fn get_device_nick_name(&self, name: sf::OutMapAliasBuffer<u8>);
}

nx::ipc_client_define_client_default!(SetSysService);
impl ISetSysInterfaceClient for SetSysService {}

impl service::IService for SetSysService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("set:sys")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The console's nickname ("Living Room Switch").
pub fn nickname() -> Result<String> {
    let srv = service::new_service_object::<SetSysService>()?;
    let mut buf = [0u8; 0x80];
    srv.get_device_nick_name(sf::Buffer::from_mut_array(&mut buf))?;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
}
