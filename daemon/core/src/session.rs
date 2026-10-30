// SPDX-License-Identifier: GPL-3.0-only
//! One client connection's state and request handling.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;

use nx::fs::FileAccessor;
use proto::path::{self, PathError};
use proto::{Button, DaemonKind, ErrorKind, HekateTarget, PROTOCOL_VERSION, Request, Response, SysmoduleStatus, crc32};
use services::power::MAX_PAYLOAD_SIZE;

use crate::sd;


struct Upload {
    path: String,
    file: FileAccessor,
    size: u64,
    received: u64,
    crc: u32,
}

pub struct Session<'a> {
    token: Option<&'a str>,
    /// Where this connection stages uploads before renaming them over the target, one file per client
    /// slot so concurrent clients don't collide.
    staging: String,
    kind: DaemonKind,
    authed: bool,
    upload: Option<Upload>,
    after_reply: Option<AfterReply>,
    /// The last screenshot's JPEG, until the next one or the end of the connection.
    screenshot: Option<alloc::vec::Vec<u8>>,
}

/// Something to do once the current reply has been sent.
pub enum AfterReply {
    /// End the NRO daemon.
    Exit,
    /// NRO: reboot through ams_mitm, to the payload just installed with `bpc:ams`.
    Reboot,
    /// Sysmodule: load this payload into IRAM and reboot to it.
    RebootDirect(Box<[u8; MAX_PAYLOAD_SIZE]>),
}

impl<'a> Session<'a> {
    /// `token` is the shared secret clients must present; with `None` every Hello is refused. `slot`
    /// (below [`crate::server::MAX_CLIENTS`]) is unique among the connections being served.
    pub fn new(token: Option<&'a str>, kind: DaemonKind, slot: usize) -> Self {
        let staging = format!("{}/upload-{slot}.part", path::STAGING_DIR);
        Session { token, kind, staging, authed: false, upload: None, after_reply: None, screenshot: None }
    }

    /// What the last request asked for after its reply (exit or reboot), if anything.
    pub fn take_after_reply(&mut self) -> Option<AfterReply> {
        self.after_reply.take()
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::Hello { version, token } => self.hello(version, &token),
            _ if !self.authed => Response::error(ErrorKind::Unauthorized, "send Hello with the token first"),
            req => {
                // Any request means an agent is working: keep the screen on a while. (Not KeepAwake, which
                // may be ending the lease.)
                if !matches!(req, Request::KeepAwake { .. }) {
                    crate::awake::extend(crate::awake::ACTIVITY_LEASE_MS);
                }
                self.handle_authed(req)
            }
        }
    }

    fn handle_authed(&mut self, req: Request) -> Response {
        match req {
            Request::Hello { .. } => unreachable!("handled above"),
            Request::Ping { nonce } => Response::Pong { nonce, uptime_ms: crate::uptime_ms() },
            Request::ListDir { path } => self.list_dir(&path),
            Request::Stat { path } => match path::check_read(&path) {
                Ok(path) => sd::stat(&path).map_or_else(Response::Error, Response::Stat),
                Err(e) => forbidden(e),
            },
            Request::ReadFile { path, offset, len } => self.read_file(&path, offset, len),
            Request::UploadBegin { path, size } => self.upload_begin(&path, size),
            Request::UploadChunk { offset, data } => self.upload_chunk(offset, &data),
            Request::UploadFinish { crc32 } => self.upload_finish(crc32),
            Request::UploadAbort => {
                self.abort_upload();
                Response::Ok
            }
            Request::Delete { path, recursive } => self.delete(&path, recursive),
            Request::SysmoduleStatus => Response::Sysmodule(sysmodule_status()),
            Request::SysmoduleStart => sysmodule_start(),
            Request::SysmoduleStop => self.sysmodule_stop(),
            Request::Exit if self.kind == DaemonKind::Nro => {
                self.after_reply = Some(AfterReply::Exit);
                Response::Ok
            }
            Request::RebootToHekate { target } => self.reboot_to_hekate(target),
            Request::IramPeek { offset } => self.iram_peek(offset),
            Request::Press { buttons, times, hold_ms } => press(&buttons, times, hold_ms),
            Request::KeepAwake { minutes } => keep_awake(minutes),
            Request::Screenshot => self.screenshot(),
            Request::ScreenshotRead { offset, len } => match &self.screenshot {
                Some(jpeg) => chunk(jpeg, offset, len),
                None => Response::error(ErrorKind::BadRequest, "no screenshot taken on this connection"),
            },
            Request::IndicatorPosition { x, y } => {
                crate::tray::set_position((x, y));
                Response::Ok
            }
            Request::Resources => resources().map_or_else(|rc| system_error("reading resource limits", rc), Response::Resources),
            Request::Exit => Response::error(ErrorKind::Unsupported, "the sysmodule doesn't exit on request"),
            Request::Handover => self.handover(),
        }
    }

    fn hello(&mut self, version: u32, token: &str) -> Response {
        if version != PROTOCOL_VERSION {
            let msg = format!("daemon speaks protocol {PROTOCOL_VERSION}, client sent {version}");
            return Response::error(ErrorKind::VersionMismatch, msg);
        }
        let Some(expected) = self.token else {
            let msg = format!("no token configured: put one in {}", path::TOKEN_PATH);
            return Response::error(ErrorKind::Unauthorized, msg);
        };
        if !constant_time_eq(expected.as_bytes(), token.as_bytes()) {
            return Response::error(ErrorKind::Unauthorized, "wrong token");
        }
        self.authed = true;
        Response::Hello { version: PROTOCOL_VERSION, daemon_version: String::from(crate::VERSION), kind: self.kind }
    }

    fn list_dir(&mut self, path: &str) -> Response {
        let path = match path::check_read(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        match sd::list_dir(&path, proto::MAX_DIR_ENTRIES) {
            Ok((entries, truncated)) => Response::Dir { entries, truncated },
            Err(e) => Response::Error(e),
        }
    }

    fn read_file(&mut self, path: &str, offset: u64, len: u32) -> Response {
        let path = match path::check_read(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        match sd::read(&path, offset, (len as usize).min(proto::MAX_CHUNK)) {
            Ok((data, file_size)) => Response::Data { data, file_size },
            Err(e) => Response::Error(e),
        }
    }

    fn upload_begin(&mut self, path: &str, size: u64) -> Response {
        self.abort_upload();
        let path = match path::check_write(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        let file = sd::create_dirs(path::STAGING_DIR)
            .and_then(|()| sd::create_dirs(path::parent(&path)))
            .and_then(|()| sd::create_for_write(&self.staging, size));
        match file {
            Ok(file) => {
                crate::log!("upload {path}: {size} bytes");
                self.upload = Some(Upload { path, file, size, received: 0, crc: crc32::INIT });
                Response::UploadReady { staging_path: self.staging.clone() }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn upload_chunk(&mut self, offset: u64, data: &[u8]) -> Response {
        let Some(up) = self.upload.as_mut() else {
            return Response::error(ErrorKind::UploadState, "no upload in progress");
        };
        if offset != up.received || up.received + data.len() as u64 > up.size {
            let msg = format!("chunk at {offset}+{} but have {} of {}", data.len(), up.received, up.size);
            self.abort_upload();
            return Response::error(ErrorKind::UploadState, msg);
        }
        if let Err(rc) = up.file.write_array::<u8, false>(data) {
            let err = sd::io_error("write", &self.staging, rc);
            self.abort_upload();
            return Response::Error(err);
        }
        up.received += data.len() as u64;
        up.crc = crc32::update(up.crc, data);
        Response::ChunkOk { received: up.received }
    }

    fn upload_finish(&mut self, expected_crc: u32) -> Response {
        let Some(up) = self.upload.take() else {
            return Response::error(ErrorKind::UploadState, "no upload in progress");
        };
        if up.received != up.size {
            let msg = format!("finished after {} of {} bytes", up.received, up.size);
            drop(up);
            let _ = sd::remove_if_exists(&self.staging);
            return Response::error(ErrorKind::UploadState, msg);
        }
        let crc = crc32::finish(up.crc);
        if crc != expected_crc {
            drop(up);
            let _ = sd::remove_if_exists(&self.staging);
            let msg = format!("crc32 {crc:08x}, client expected {expected_crc:08x}");
            return Response::error(ErrorKind::ChecksumMismatch, msg);
        }
        let Upload { path, file, size, .. } = up;
        if let Err(rc) = file.flush() {
            return Response::Error(sd::io_error("flush", &self.staging, rc));
        }
        drop(file);
        let result = sd::remove_if_exists(&path).and_then(|()| sd::rename(&self.staging, &path));
        match result {
            Ok(()) => {
                crate::log!("upload {path}: done");
                Response::UploadDone { path, size }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn delete(&mut self, path: &str, recursive: bool) -> Response {
        // The staging file lives in a deletable directory; don't pull it out from under an upload.
        if self.upload.is_some() {
            return Response::error(ErrorKind::UploadState, "finish or abort the upload before deleting");
        }
        let path = match path::check_write(path) {
            Ok(p) => p,
            Err(e) => return forbidden(e),
        };
        match sd::delete(&path, recursive) {
            Ok(was_dir) => {
                crate::log!("deleted {path}");
                Response::Deleted { path, was_dir }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn sysmodule_stop(&mut self) -> Response {
        if self.kind == DaemonKind::Sysmodule {
            return Response::error(ErrorKind::Unsupported, "the sysmodule can't stop itself; use the NRO daemon");
        }
        if sysmodule_status().pid.is_some()
            && let Err(rc) = services::pm::terminate_program(path::SYSMODULE_PROGRAM_ID)
        {
            return system_error("terminating the sysmodule", rc);
        }
        crate::log!("sysmodule stopped");
        Response::Sysmodule(sysmodule_status())
    }

    fn handover(&mut self) -> Response {
        if self.kind != DaemonKind::Nro {
            return Response::error(ErrorKind::Unsupported, "only the NRO daemon hands over");
        }
        let status = sysmodule_status();
        if !status.installed {
            return Response::error(ErrorKind::NotFound, format!("not installed: upload it to {}", path::SYSMODULE_NSP));
        }
        // An old instance would hold or contend for the port; replace it.
        if status.pid.is_some()
            && let Err(rc) = services::pm::terminate_program(path::SYSMODULE_PROGRAM_ID)
        {
            return system_error("stopping the running sysmodule", rc);
        }
        crate::HANDOVER_REQUESTED.store(true, core::sync::atomic::Ordering::Relaxed);
        self.after_reply = Some(AfterReply::Exit);
        Response::Ok
    }

    /// Prepares hekate with a boot config for `target` as the reboot payload; the reboot itself happens
    /// after the reply. Everything that can fail short of the reboot call is checked here.
    fn reboot_to_hekate(&mut self, target: HekateTarget) -> Response {
        let next = match self.kind {
            DaemonKind::Nro => install_hekate(target).map(|()| AfterReply::Reboot),
            DaemonKind::Sysmodule => build_hekate(target).map(AfterReply::RebootDirect),
        };
        // Booting straight back into an entry: have the sysmodule get past the lock screen when it's up.
        // (After the menu or UMS, whoever picks what boots next is at the console.)
        let next = next.and_then(|next| match target {
            HekateTarget::Entry(_) => crate::awake::mark_agent_reboot().map(|()| next).map_err(Response::Error),
            _ => Ok(next),
        });
        match next {
            Ok(next) => {
                crate::log!("rebooting to hekate: {target:?}");
                self.after_reply = Some(next);
                Response::Ok
            }
            Err(resp) => resp,
        }
    }

    fn iram_peek(&self, offset: u32) -> Response {
        use services::power::{IRAM_SIZE, PAGE_SIZE, Page};
        if self.kind != DaemonKind::Sysmodule {
            return Response::error(ErrorKind::Unsupported, "only the sysmodule may call the secure monitor");
        }
        let offset = offset as usize;
        if !offset.is_multiple_of(PAGE_SIZE) || offset >= IRAM_SIZE {
            return Response::error(ErrorKind::BadRequest, format!("offset must be page aligned and below {IRAM_SIZE:#x}"));
        }
        let mut page = Box::new(Page([0; PAGE_SIZE]));
        match services::power::iram_read(offset, &mut page) {
            Ok(()) => Response::Data { data: page.0.to_vec(), file_size: IRAM_SIZE as u64 },
            Err(e) => Response::error(ErrorKind::System, format!("IramCopy failed: SmcResult {e}")),
        }
    }

    fn screenshot(&mut self) -> Response {
        // Free the last one first: the heap has room for one capture buffer, not two.
        self.screenshot = None;
        let mut buf = alloc::vec::Vec::new();
        if buf.try_reserve_exact(services::caps::JPEG_BUFFER_SIZE).is_err() {
            return Response::error(ErrorKind::OutOfMemory, "no room for a 512 KiB capture buffer right now");
        }
        buf.resize(services::caps::JPEG_BUFFER_SIZE, 0);
        match services::caps::capture_jpeg(&mut buf) {
            Ok(size) if size <= buf.len() => {
                buf.truncate(size);
                buf.shrink_to_fit();
                let first = chunk(&buf, 0, proto::MAX_CHUNK as u32);
                self.screenshot = Some(buf);
                first
            }
            Ok(size) => Response::error(ErrorKind::System, format!("caps:sc reported a {size} byte JPEG")),
            Err(rc) => system_error("capturing the screen (caps:sc)", rc),
        }
    }

    fn abort_upload(&mut self) {
        if self.upload.take().is_some() {
            let _ = sd::remove_if_exists(&self.staging);
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.abort_upload();
    }
}

/// `len` bytes (at most [`proto::MAX_CHUNK`]) of `data` from `offset`, as [`Response::Data`].
fn chunk(data: &[u8], offset: u64, len: u32) -> Response {
    let start = (offset as usize).min(data.len());
    let end = start.saturating_add((len as usize).min(proto::MAX_CHUNK)).min(data.len());
    Response::Data { data: data[start..end].to_vec(), file_size: data.len() as u64 }
}

/// Longest hold [`Request::Press`] accepts; with [`MAX_PRESSES`] it keeps the call under the client's 30 s
/// reply timeout.
const MAX_HOLD_MS: u32 = 2_000;
/// Most presses in one [`Request::Press`].
const MAX_PRESSES: u8 = 10;

fn keep_awake(minutes: Option<u32>) -> Response {
    match minutes {
        Some(0) => crate::awake::clear(),
        Some(m) => crate::awake::extend((u64::from(m) * 60_000).min(crate::awake::MAX_LEASE_MS)),
        None => {}
    }
    Response::KeepAwake { remaining_s: crate::awake::remaining_ms() / 1000 }
}

fn press(buttons: &[Button], times: u8, hold_ms: u32) -> Response {
    if buttons.is_empty() || times == 0 || times > MAX_PRESSES || hold_ms == 0 || hold_ms > MAX_HOLD_MS {
        let msg = format!("need at least one button, 1-{MAX_PRESSES} presses and a 1-{MAX_HOLD_MS} ms hold");
        return Response::error(ErrorKind::BadRequest, msg);
    }
    let bits = buttons.iter().fold(0, |bits, b| bits | b.bit());
    crate::log!("press {buttons:?} x{times}");
    match services::hid::press(bits, u32::from(times), hold_ms) {
        Ok(()) => Response::Ok,
        Err(rc) => system_error("pressing buttons (hid:dbg)", rc),
    }
}

fn sysmodule_status() -> SysmoduleStatus {
    SysmoduleStatus {
        installed: sd::stat(path::SYSMODULE_NSP).is_ok(),
        boot2: sd::stat(path::SYSMODULE_BOOT2_FLAG).is_ok(),
        pid: services::pm::process_id(path::SYSMODULE_PROGRAM_ID),
    }
}

fn sysmodule_start() -> Response {
    let status = sysmodule_status();
    if status.pid.is_some() {
        return Response::Sysmodule(status);
    }
    if !status.installed {
        return Response::error(ErrorKind::NotFound, format!("not installed: upload it to {}", path::SYSMODULE_NSP));
    }
    match crate::start_sysmodule() {
        Ok(pid) => {
            crate::log!("sysmodule started, pid {pid}");
            Response::Sysmodule(SysmoduleStatus { pid: Some(pid), ..status })
        }
        Err(rc) => system_error("launching the sysmodule", rc),
    }
}

/// Reads hekate from [`path::REBOOT_PAYLOAD`] and writes the boot config for `target` into it.
/// [`HekateTarget::Menu`] leaves the boot config empty, which is the file as it is on the card.
fn build_hekate(target: HekateTarget) -> Result<Box<[u8; MAX_PAYLOAD_SIZE]>, Response> {
    let file_size = sd::stat(path::REBOOT_PAYLOAD).map_err(Response::Error)?.size as usize;
    if file_size > MAX_PAYLOAD_SIZE {
        let msg = format!("{} is {file_size} bytes; Atmosphère takes at most {MAX_PAYLOAD_SIZE}", path::REBOOT_PAYLOAD);
        return Err(Response::error(ErrorKind::BadRequest, msg));
    }
    let mut payload: Box<[u8; MAX_PAYLOAD_SIZE]> = vec![0u8; MAX_PAYLOAD_SIZE].into_boxed_slice().try_into().unwrap();
    let mut done = 0;
    while done < file_size {
        let (data, _) = sd::read(path::REBOOT_PAYLOAD, done as u64, (file_size - done).min(proto::MAX_CHUNK))
            .map_err(Response::Error)?;
        if data.is_empty() {
            break;
        }
        payload[done..done + data.len()].copy_from_slice(&data);
        done += data.len();
    }
    if done != file_size {
        return Err(Response::error(ErrorKind::Io, format!("{} ended after {done} of {file_size} bytes", path::REBOOT_PAYLOAD)));
    }
    if !proto::hekate::is_hekate(&payload[..done]) {
        let msg = format!("{} isn't hekate (no ICTC magic), refusing to reboot into it", path::REBOOT_PAYLOAD);
        return Err(Response::error(ErrorKind::BadRequest, msg));
    }
    payload[proto::hekate::BOOT_CFG_OFFSET..proto::hekate::MAGIC_OFFSET].copy_from_slice(&proto::hekate::boot_cfg(target));
    Ok(payload)
}

/// [`build_hekate`], then hands the payload to Atmosphère (`bpc:ams`) for a redirected reboot.
pub fn install_hekate(target: HekateTarget) -> Result<(), Response> {
    let payload = build_hekate(target)?;
    services::power::set_reboot_payload(&payload).map_err(|rc| system_error("setting the reboot payload (bpc:ams)", rc))
}

/// This process's resource limit usage (shared with the rest of its group) and its own memory.
fn resources() -> nx::result::Result<proto::Resources> {
    use nx::svc::{self, InfoId, LimitableResource};
    let me = svc::CURRENT_PROCESS_PSEUDO_HANDLE;
    let limit = svc::get_info(InfoId::ResourceLimit, 0, 0)? as svc::Handle;
    let kinds = [
        (LimitableResource::Memory, "memory"),
        (LimitableResource::Threads, "threads"),
        (LimitableResource::Events, "events"),
        (LimitableResource::TransferMemories, "transfer memories"),
        (LimitableResource::Sessions, "sessions"),
    ];
    let mut limits = alloc::vec::Vec::new();
    let result: nx::result::Result<()> = (|| {
        for (kind, name) in kinds {
            let mut peak = 0;
            nx::result::pack(unsafe { svc::asm::get_resource_limit_peak_value(&mut peak, limit, kind) }, ())?;
            limits.push(proto::ResourceUsage {
                name: name.into(),
                current: svc::get_resource_limit_current_value(limit, kind)?,
                peak,
                limit: svc::get_resource_limit_limit_value(limit, kind)?,
            });
        }
        Ok(())
    })();
    let _ = svc::close_handle(limit);
    result?;
    Ok(proto::Resources {
        limits,
        process_used: svc::get_info(InfoId::UsedMemorySize, me, 0)?,
        process_total: svc::get_info(InfoId::TotalMemorySize, me, 0)?,
    })
}

fn system_error(what: &str, rc: nx::result::ResultCode) -> Response {
    let message = format!("{what}: {rc}");
    Response::Error(proto::Error { kind: ErrorKind::System, message, result: Some(rc.get_value()) })
}

fn forbidden(e: PathError) -> Response {
    let kind = match e {
        PathError::ReadDenied | PathError::WriteDenied => ErrorKind::Forbidden,
        _ => ErrorKind::BadRequest,
    };
    Response::error(kind, e.describe())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
