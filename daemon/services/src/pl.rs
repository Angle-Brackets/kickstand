// SPDX-License-Identifier: GPL-3.0-only
//! `pl:u`: the system's shared fonts, the ones the home menu draws with, so the NRO's screen looks like the
//! rest of the console without shipping a font. Command ids, the shared memory size and layout (each font
//! a TrueType file after an 8-byte header) from libnx `source/services/pl.c` (ISC).

use nx::ipc::sf::{self, sm};
use nx::result::Result;
use nx::{service, svc, vmem};

#[nx_derive::ipc_trait]
pub trait PlInterface {
    #[ipc_rid(0)]
    fn request_load(&self, font_type: u32);
    #[ipc_rid(1)]
    fn get_load_state(&self, font_type: u32) -> u32;
    #[ipc_rid(2)]
    fn get_size(&self, font_type: u32) -> u32;
    #[ipc_rid(3)]
    fn get_shared_memory_address_offset(&self, font_type: u32) -> u32;
    #[ipc_rid(4)]
    fn get_shared_memory_native_handle(&self) -> sf::CopyHandle;
}

nx::ipc_client_define_client_default!(PlService);
impl IPlInterfaceClient for PlService {}

impl service::IService for PlService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("pl:u")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// `PlSharedFontType_Standard`: Latin, Greek, Cyrillic, Japanese.
pub const FONT_STANDARD: u32 = 0;
/// `PlSharedFontType_NintendoExt`: the button and system glyphs (U+E0E0 Ⓐ and on).
pub const FONT_NINTENDO_EXT: u32 = 5;

const SHARED_MEMORY_SIZE: usize = 0x110_0000;
const LOADED: u32 = 1;

/// The system's shared fonts, mapped while this lives. Dropping it unmaps them and closes the session:
/// an NRO must, as hbloader runs the next one (hbmenu) in the same process, and libnx's pl:u client there
/// needs the session back.
pub struct SharedFonts {
    base: *const u8,
    handle: svc::Handle,
    srv: PlService,
}

impl SharedFonts {
    pub fn map() -> Result<Self> {
        let srv = service::new_service_object::<PlService>()?;
        let handle = srv.get_shared_memory_native_handle()?;
        let base = vmem::allocate(SHARED_MEMORY_SIZE)?;
        if let Err(rc) = unsafe { svc::map_shared_memory(handle.handle, base, SHARED_MEMORY_SIZE, svc::MemoryPermission::Read()) } {
            let _ = svc::close_handle(handle.handle);
            return Err(rc);
        }
        Ok(SharedFonts { base, handle: handle.handle, srv })
    }

    /// Font `font_type`'s TrueType data, once pl has it loaded.
    pub fn font(&self, font_type: u32) -> Result<&[u8]> {
        self.srv.request_load(font_type)?;
        for _ in 0..100 {
            if self.srv.get_load_state(font_type)? == LOADED {
                break;
            }
            let _ = svc::sleep_thread(10_000_000);
        }
        let offset = self.srv.get_shared_memory_address_offset(font_type)? as usize;
        let size = self.srv.get_size(font_type)? as usize;
        // The offset is the font's own; the 8-byte header sits before it. Should it ever point at the header
        // instead, the TrueType version tag shows which.
        let skip = if self.at(offset, 4) == [0, 1, 0, 0] { 0 } else { 8 };
        let skip = if offset + skip + size > SHARED_MEMORY_SIZE { 0 } else { skip };
        Ok(self.at(offset + skip, size))
    }

    fn at(&self, offset: usize, len: usize) -> &[u8] {
        let len = len.min(SHARED_MEMORY_SIZE.saturating_sub(offset));
        // Mapped until `self` drops.
        unsafe { core::slice::from_raw_parts(self.base.add(offset.min(SHARED_MEMORY_SIZE)), len) }
    }
}

impl Drop for SharedFonts {
    fn drop(&mut self) {
        unsafe {
            let _ = svc::unmap_shared_memory(self.handle, self.base, SHARED_MEMORY_SIZE);
        }
        let _ = svc::close_handle(self.handle);
    }
}
