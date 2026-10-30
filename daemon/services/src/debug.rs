// SPDX-License-Identifier: GPL-3.0-only
//! Another process's memory, read and written as its debugger: svcDebugActiveProcess, which the
//! sysmodule may call on any process thanks to the `force_debug` kernel capability. [`Debugged`] detaches
//! when dropped (closing the debug handle), which lets the process run on; nothing stays attached between
//! requests, so a crash later still reaches creport. Layouts from libnx `svc.h`.

use alloc::vec::Vec;

use nx::result::Result;
use nx::svc::{self, Handle};

/// `MemoryInfo` with plain integers: nx's enum lacks the newest kernel memory types.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct MemoryInfo {
    pub base_address: u64,
    pub size: u64,
    /// `MemoryType` (the low byte of the kernel's state).
    pub state: u32,
    pub attribute: u32,
    /// Bit 0 read, 1 write, 2 execute.
    pub permission: u32,
    pub ipc_refcount: u32,
    pub device_refcount: u32,
    pub pad: u32,
}

/// `MemoryType_Normal`: heap.
pub const STATE_HEAP: u32 = 5;

pub struct Debugged(Handle);

impl Debugged {
    pub fn attach(pid: u64) -> Result<Self> {
        svc::debug_active_process(pid).map(Debugged)
    }

    pub fn query(&self, address: u64) -> Result<MemoryInfo> {
        let mut info = MemoryInfo::default();
        let mut page = 0;
        // Same layout as nx's `MemoryInfo`, whose enums can't hold every value the kernel writes.
        let rc = unsafe { svc::asm::query_debug_process_memory((&raw mut info).cast(), &mut page, self.0, address as usize) };
        nx::result::pack(rc, info)
    }

    /// Up to `max` mapped regions (any permission) in address order, and whether there were more.
    pub fn mappings(&self, max: usize) -> Result<(Vec<MemoryInfo>, bool)> {
        let mut out = Vec::new();
        let mut address = 0u64;
        loop {
            let info = self.query(address)?;
            if info.permission != 0 {
                if out.len() == max {
                    return Ok((out, true));
                }
                out.push(info);
            }
            address = info.base_address.wrapping_add(info.size);
            if address == 0 || info.size == 0 {
                return Ok((out, false));
            }
        }
    }

    pub fn read(&self, address: u64, out: &mut [u8]) -> Result<()> {
        unsafe { svc::read_debug_process_memory(self.0, address as usize, out) }
    }

    pub fn write(&self, address: u64, data: &[u8]) -> Result<()> {
        unsafe { svc::write_debug_process_memory(self.0, address as usize, data.len(), data.as_ptr()) }
    }
}

impl Drop for Debugged {
    fn drop(&mut self) {
        let _ = svc::close_handle(self.0);
    }
}
