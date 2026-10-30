// SPDX-License-Identifier: GPL-3.0-only
//! `ns:am2`: the installed applications and their names. Ids and layouts from libnx
//! `source/services/ns.c` and `include/switch/services/ns.h` (3.0+ and 5.1+ forms) and `nacp.h`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::service;

/// `NsApplicationRecord`.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ApplicationRecord {
    pub application_id: u64,
    pub record_type: u8,
    pub unk: [u8; 15],
}

#[nx_derive::ipc_trait]
#[default_client]
pub trait ApplicationManager {
    /// `nsListApplicationRecord`: records from `offset`, most recently used first (the home menu's order).
    #[ipc_rid(0)]
    fn list_application_record(&self, offset: i32, records: sf::OutMapAliasBuffer<ApplicationRecord>) -> i32;
}

#[nx_derive::ipc_trait]
#[default_client]
pub trait ReadOnlyApplicationControlData {
    /// `nsGetApplicationControlData` on 5.1+: the NACP then the icon; returns the size written.
    #[ipc_rid(0)]
    fn get_application_control_data(&self, args: ControlDataArgs, data: sf::OutMapAliasBuffer<u8>) -> u32;
}

#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct ControlDataArgs {
    /// `NsApplicationControlSource`: 0 cache or storage, 1 storage, 2 storage only.
    pub source: u8,
    pub pad: [u8; 7],
    pub application_id: u64,
}

#[nx_derive::ipc_trait]
pub trait Getter {
    #[ipc_rid(7989)]
    #[return_session]
    fn get_read_only_application_control_data_interface(&self) -> ReadOnlyApplicationControlData;
    #[ipc_rid(7996)]
    #[return_session]
    fn get_application_manager_interface(&self) -> ApplicationManager;
}

nx::ipc_client_define_client_default!(GetterService);
impl IGetterClient for GetterService {}

impl service::IService for GetterService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("ns:am2")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// `NsApplicationControlData`: the 0x4000-byte NACP, then a JPEG icon of up to 0x20000.
const CONTROL_DATA_SIZE: usize = 0x24000;
const NACP_SIZE: usize = 0x4000;
/// `NacpLanguageEntry`: a 0x200-byte name then a 0x100-byte author, 16 of them at the NACP's start.
const LANGUAGE_ENTRY_SIZE: usize = 0x300;
const NAME_SIZE: usize = 0x200;
const LANGUAGES: usize = 16;

/// Installed application ids, most recently used first, at most `max`.
pub fn application_ids(max: usize) -> Result<Vec<u64>> {
    let manager = service::new_service_object::<GetterService>()?.get_application_manager_interface()?;
    let mut records = vec![ApplicationRecord::default(); max];
    let count = manager.list_application_record(0, sf::Buffer::from_mut_array(&mut records))?;
    records.truncate(count.clamp(0, max as i32) as usize);
    Ok(records.iter().map(|r| r.application_id).collect())
}

/// Reads applications' names (the first language the NACP fills in, normally American English).
pub struct Names {
    control: ReadOnlyApplicationControlData,
    buf: Vec<u8>,
}

impl Names {
    pub fn new() -> Result<Self> {
        let control = service::new_service_object::<GetterService>()?.get_read_only_application_control_data_interface()?;
        Ok(Names { control, buf: vec![0; CONTROL_DATA_SIZE] })
    }

    /// Empty if the title has no readable NACP (an archived title, a missing update).
    pub fn get(&mut self, application_id: u64) -> Result<String> {
        let args = ControlDataArgs { source: 0, pad: [0; 7], application_id };
        // Cleared first: a short answer would otherwise leave the previous title's bytes.
        self.buf[..NACP_SIZE].fill(0);
        let size = self.control.get_application_control_data(args, sf::Buffer::from_mut_array(&mut self.buf))?;
        if (size as usize) < NACP_SIZE {
            return Ok(String::new());
        }
        let name = self.buf[..LANGUAGES * LANGUAGE_ENTRY_SIZE]
            .as_chunks::<LANGUAGE_ENTRY_SIZE>()
            .0
            .iter()
            .map(|entry| &entry[..NAME_SIZE])
            .find(|name| name[0] != 0)
            .unwrap_or(&[]);
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Ok(core::str::from_utf8(&name[..end]).map(String::from).unwrap_or_default())
    }
}
