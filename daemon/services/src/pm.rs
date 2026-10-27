// SPDX-License-Identifier: GPL-3.0-only
//! Process manager: `pm:shell` (launch and terminate programs) and the `pm:dmnt` command nx lacks.
//! Layouts from libnx `source/services/pm.c` and `ncm_types.h`.

use nx::ipc::sf::ncm::{ProgramId, StorageId};
use nx::ipc::sf::sm;
use nx::result::Result;
use nx::service;

/// `NcmProgramLocation`. SD-card sysmodules (Atmosphère `contents/`) launch with [`StorageId::None`].
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct ProgramLocation {
    pub program_id: ProgramId,
    pub storage_id: StorageId,
    pub pad: [u8; 7],
}

/// The input of `pmshellLaunchProgram`.
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct LaunchArgs {
    pub launch_flags: u32,
    pub pad: u32,
    pub location: ProgramLocation,
}

#[nx_derive::ipc_trait]
pub trait ShellInterface {
    /// `pmshellLaunchProgram`: returns the new process id.
    #[ipc_rid(0)]
    fn launch_program(&self, args: LaunchArgs) -> u64;
    /// `pmshellTerminateProgram`.
    #[ipc_rid(2)]
    fn terminate_program(&self, program_id: ProgramId);
}

nx::ipc_client_define_client_default!(ShellService);
impl IShellInterfaceClient for ShellService {}

impl service::IService for ShellService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("pm:shell")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

#[nx_derive::ipc_trait]
pub trait DebugMonitorExtInterface {
    /// `pmdmntGetProcessId` (id 2 on 5.0+): the process running `program_id`, or an error if none is.
    #[ipc_rid(2)]
    fn get_process_id(&self, program_id: ProgramId) -> u64;
}

nx::ipc_client_define_client_default!(DebugMonitorExtService);
impl IDebugMonitorExtInterfaceClient for DebugMonitorExtService {}

impl service::IService for DebugMonitorExtService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("pm:dmnt")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Starts a program installed on the SD card (`atmosphere/contents/<id>/exefs.nsp`). Returns its pid.
pub fn launch_sd_program(program_id: u64) -> Result<u64> {
    let shell = service::new_service_object::<ShellService>()?;
    let location = ProgramLocation { program_id: ProgramId(program_id), storage_id: StorageId::None, pad: [0; 7] };
    shell.launch_program(LaunchArgs { launch_flags: 0, pad: 0, location })
}

pub fn terminate_program(program_id: u64) -> Result<()> {
    service::new_service_object::<ShellService>()?.terminate_program(ProgramId(program_id))
}

/// The pid running `program_id`, or `None` if it isn't running.
pub fn process_id(program_id: u64) -> Option<u64> {
    let dmnt = service::new_service_object::<DebugMonitorExtService>().ok()?;
    dmnt.get_process_id(ProgramId(program_id)).ok()
}
