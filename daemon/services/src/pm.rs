// SPDX-License-Identifier: GPL-3.0-only
//! Process manager: `pm:shell` (launch and terminate programs), the `pm:dmnt` and `pm:info` commands nx
//! lacks, and Atmosphère's `pm:info` extension that says whether a process runs under hbl. Layouts from
//! libnx `source/services/pm.c` and `ncm_types.h`; the extension's id and output from Atmosphère's
//! `pm_info_service.hpp` and `cfg_types.hpp` (layout only).

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
    /// `pmshellTerminateProcess`.
    #[ipc_rid(1)]
    fn terminate_process(&self, pid: u64);
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
    /// `pmdmntGetApplicationProcessId` (id 4 on 5.0+): the running application, or an error if none is.
    #[ipc_rid(4)]
    fn get_application_process_id(&self) -> u64;
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

/// What Atmosphère's `AtmosphereGetProcessInfo` reports: where the program was loaded from, and
/// `cfg::OverrideStatus` (the keys held at launch and the override flags).
#[derive(nx_derive::Request, nx_derive::Response, Copy, Clone, Default)]
#[repr(C)]
pub struct ProcessInfo {
    pub location: ProgramLocation,
    pub keys_held: u64,
    pub override_flags: u64,
}

/// `OverrideStatusFlag_Hbl`: the process is hbloader, standing in for the title it was launched as.
const OVERRIDE_HBL: u64 = 1 << 0;

#[nx_derive::ipc_trait]
pub trait InfoInterface {
    /// `pminfoGetProgramId`.
    #[ipc_rid(0)]
    fn get_program_id(&self, pid: u64) -> ProgramId;
    /// Atmosphère's `AtmosphereGetProcessInfo`.
    #[ipc_rid(65002)]
    fn atmosphere_get_process_info(&self, pid: u64) -> ProcessInfo;
}

nx::ipc_client_define_client_default!(InfoService);
impl IInfoInterfaceClient for InfoService {}

impl service::IService for InfoService {
    fn get_name() -> sm::ServiceName {
        sm::ServiceName::new("pm:info")
    }

    fn as_domain() -> bool {
        false
    }

    fn post_initialize(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The application in the foreground slot (a game, or hbloader in its place).
pub struct Application {
    pub pid: u64,
    pub program_id: u64,
    /// Running as hbloader (hbmenu or an NRO), `None` if Atmosphère wouldn't say.
    pub hbl: Option<bool>,
}

/// The running application, if any.
pub fn application() -> Result<Option<Application>> {
    let dmnt = service::new_service_object::<DebugMonitorExtService>()?;
    // pm answers with an error when no application is running.
    let Ok(pid) = dmnt.get_application_process_id() else { return Ok(None) };
    let info = service::new_service_object::<InfoService>()?;
    let program_id = info.get_program_id(pid)?.0;
    let hbl = info.atmosphere_get_process_info(pid).ok().map(|p| p.override_flags & OVERRIDE_HBL != 0);
    Ok(Some(Application { pid, program_id, hbl }))
}

pub fn terminate_process(pid: u64) -> Result<()> {
    service::new_service_object::<ShellService>()?.terminate_process(pid)
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
