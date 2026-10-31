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
/// [`MemoryInfo::permission`]'s execute bit.
pub const PERM_EXECUTE: u32 = 4;

/// A paused thread's registers.
pub struct Registers {
    pub pc: u64,
    pub lr: u64,
    pub sp: u64,
    pub fp: u64,
}

// libnx's `ThreadContext` is 0x320 bytes; a different size means nx's layout is off.
const _: () = assert!(core::mem::size_of::<nx::arm::ThreadContext>() == 0x320);

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
    /// Neighbours with the same type and permission are one region (a busy heap is hundreds of blocks
    /// otherwise), and executable regions are kept past `max`, so the code (an NRO's header) is always there.
    pub fn mappings(&self, max: usize) -> Result<(Vec<MemoryInfo>, bool)> {
        let mut out: Vec<MemoryInfo> = Vec::new();
        let mut truncated = false;
        let mut address = 0u64;
        loop {
            let info = self.query(address)?;
            if info.permission != 0 {
                let full = out.len() >= max;
                match out.last_mut() {
                    Some(last)
                        if last.base_address + last.size == info.base_address
                            && (last.state, last.permission) == (info.state, info.permission) =>
                    {
                        last.size += info.size;
                    }
                    _ if full && info.permission & PERM_EXECUTE == 0 => truncated = true,
                    _ => out.push(info),
                }
            }
            address = info.base_address.wrapping_add(info.size);
            if address == 0 || info.size == 0 {
                return Ok((out, truncated));
            }
        }
    }

    /// Stops every thread of the process. Closing the handle (dropping this) lets them run again.
    pub fn pause(&self) -> Result<()> {
        svc::break_debug_process(self.0)
    }

    /// The ids of up to `max` of the process's threads.
    pub fn threads(&self, max: usize) -> Result<Vec<u64>> {
        let mut ids = alloc::vec![0u64; max];
        let n = svc::get_thread_list(self.0, &mut ids)?;
        ids.truncate(n);
        Ok(ids)
    }

    /// Thread `id`'s registers; the process must be paused.
    pub fn registers(&self, id: u64) -> Result<Registers> {
        let groups = nx::arm::RegisterGroup::CpuGprs() | nx::arm::RegisterGroup::CpuSprs();
        let ctx = svc::get_debug_thread_context(self.0, id, groups)?;
        Ok(Registers { pc: ctx.pc.reg, lr: ctx.lr, sp: ctx.sp, fp: ctx.fp })
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
