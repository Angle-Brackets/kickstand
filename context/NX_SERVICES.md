# nx coverage: what we use, what we must wrap

Checked against the vendored nx (`third_party/nx`, upstream `5e0a67c`). Function names below are libnx's
(ISC), used only as the reference for command ids and struct layouts. Note the source of each layout in the
wrapper's doc comment.

## Writing a wrapper
nx's IPC macros expand to `::nx::...` paths, so wrappers live in our own crate (planned: `daemon/services`)
without patching nx. Add `nx-derive` as a dependency. A `pm:shell` wrapper in this shape compiled cleanly:

```rust
#[nx_derive::ipc_trait]
pub trait ShellInterface {
    #[ipc_rid(0)] fn launch_program(&self, args: LaunchArgs) -> u64;   // LaunchArgs: #[derive(nx_derive::Request)] #[repr(C)]
    #[ipc_rid(2)] fn terminate_program(&self, program_id: ncm::ProgramId);
}
nx::ipc_client_define_client_default!(ShellService);
impl IShellInterfaceClient for ShellService {}
impl service::IService for ShellService { /* get_name() = "pm:shell", as_domain() = false */ }
```

## Already in nx (enough for v0)
| Need | nx |
| - | - |
| SD card | `fs` (fsp-srv): open/read/write/create/rename/remove/list |
| Sockets | `socket` (bsd:u / bsd:s), TcpListener/TcpStream, poll |
| Firmware version | set:sys `get_firmware_version` |
| Process ids | pm:dmnt `get_application_process_id`, `get_program_id` (14.0+); pm:info `get_program_id` |
| Memory r/w (step 4+) | svc `debug_active_process`, `read/write_debug_process_memory`, `query_debug_process_memory`, `get_thread_list`, debug thread contexts |
| Sleep/wake notifications | psc:m `PmModule` |
| Own buttons (NRO build) | `input` (hid) |

## Missing: wrappers we need, by build step
| Step | Service | Commands (libnx name) | Notes |
| - | - | - | - |
| 3 | `pm:shell` | `pmshellLaunchProgram` (0), `TerminateProcess` (1), `TerminateProgram` (2), `GetApplicationProcessIdForShell` (6 on 5.0+) | Force-close a hung game; start our own sysmodule by hand while boot2 is off |
| 3 | `pgl` (10.0+) | `pglLaunchProgram`, `pglTerminateProcess`, `pglGetApplicationProcessId` | Newer front end over pm; pm:shell still works on Atmosphère |
| 3 | `ns:dev` | `nsdevLaunchApplicationForDevelop`, `nsdevTerminateApplication`, `nsdevGetRunningApplicationProcessIdForDevelop` | Launch a game title, which Atmosphère's hbl override turns into hbmenu/our NRO with full memory. The least proven piece |
| 3 | `bpc` | `bpcRebootSystem`, `bpcShutdownSystem` | Atmosphère's bpc mitm sends reboots to `reboot_payload.bin` (usually hekate) unless configured otherwise: check the user's setup before relying on it |
| 3 | `bpc:ams` (Atmosphère) | `SetRebootPayload` (amsbpc in libstratosphere) | Reboot to hekate with a chosen boot config (DESIGN.md, step 3). Check the command id and payload size limit in Atmosphère's source |
| 3 | `set:sys` (extend) | `setsysGetSleepSettings` / `setsysSetSleepSettings` | Turn auto-sleep off for a session, restore after |
| 3 | `nifm` | `nifmGetCurrentIpAddress` | Report the console's IP; wait for Wi-Fi after wake |
| 4 | `caps:sc` | `capsscCaptureJpegScreenShot` (9.0+, ~0x80000 buffer) or `capsscOpenRawScreenShotReadStream` / `Read…` / `Close…` | Raw 1280x720 RGBA is 0x384000 bytes, too big to hold in the heap, but the read stream gives it in chunks; libnx says raw needs debug mode, so JPEG may be the practical path. Try both |
| 4 | `hid:dbg` | `hiddbgAttachHdlsWorkBuffer` (7.0+), `AttachHdlsVirtualDevice`, `SetHdlsState`, `DetachHdlsVirtualDevice` | Virtual controller. The work buffer is transfer memory from our heap |
| 5 | `pm:dmnt` (extend) | `pmdmntHookToCreateApplicationProcess`, `GetProcessId` | Notice when the app (re)starts, for the watchdog |
| 6 | Atmosphère gdbstub | — | Not an IPC service: `enable_standalone_gdbstub` in system_settings.ini; the PC side talks GDB remote protocol |

## nx quirks found so far
- **argv**: rrt0 ignored the HBL `Argv` entry. Patched (`hbl::get_argv`, see `third_party/nx/PATCHES.md`); worth upstreaming.
- **poll**: nx's `bsd` poll request lacked the fd count, so poll never reported ready fds. Patched
  (`PATCHES.md` #2); worth upstreaming. Check other bsd commands against libnx before relying on them.
- **hbmenu argv** is quoted per argument; split it like libnx (`nxlink::split_argv`).
- **`SocketCommon::try_clone`** copies the fd, and both copies close it on drop. Don't use it.
- **Sysmodule heap**: the default `configure_heap` takes all free memory. Sysmodules must set
  `alloc_set_default_heap_size!` (ours: 4 MiB).
- **Toolchain**: `unwinding` 0.2.10 (an nx dependency) calls `core::intrinsics::abort`, which recent nightlies
  removed; nightly-2026-10-20 fails, nightly-2026-09-08 works.
