# Vendored nx

Upstream: https://github.com/aarch64-switch-rs/nx (MIT, see LICENSE), commit
`5e0a67c630352c93414b421a77ed2de587f61393` (tag 0.6.0 + 3 commits). Only `src/`, `nx-derive/`, `Cargo.toml`,
`LICENSE`, `README.md` and `changelog.md` are copied.

Local changes (worth upstreaming):
1. `hbl::get_argv()`: rrt0 now stores the HBL `Argv` ABI entry instead of ignoring it (it was a `todo!`).
   nxlink passes the PC's address through argv, so stdout over nxlink needs this.
2. `bsd` `poll` (command 6): the request now sends the fd count before the timeout, as libnx's `bsdPoll` does
   (`{ nfds, timeout }`). Without it the service read the timeout as the count and never reported a ready
   fd, so a listener polled for readability never saw incoming connections. Found on hardware (fw 23.0.1).
3. `TcpListener::bind` and `TcpStream::connect` close their socket when a later step (setsockopt, bind,
   listen, connect) fails; they used to leak it. Seen on hardware: a sysmodule retrying `bind` while the
   port was taken got EADDRINUSE once, then only EAFNOSUPPORT (2017-1097) on every retry.
4. rrt0 reads `DEFAULT_HEAP_SIZE` with a volatile read. The weak static's default (0) was folded into the
   startup code, so `alloc_set_default_heap_size!` had no effect and a sysmodule's heap took all free memory
   minus 2 MiB. Found on hardware: started at boot, our sysmodule took the System pool's slack and am
   fataled with 2001-0132 (LimitReached); started later it held 16 MiB with a 4 MiB setting.
5. `ApplicationProxy::get_application_functions` (cmd 20) and an `ApplicationFunctions` interface with
   `create_application_and_request_to_start` (cmd 12, 4.0+), from libnx `applet.c`. The kickstand NRO uses it to
   launch an installed title from inside the hbl app session.
6. `sync::sys::Mutex` locks and unlocks as libnx's `mutexLock`/`mutexUnlock` do (the algorithm; libnx is ISC).
   `lock_contended` only waited in the kernel when the wait flag wasn't set yet, so a second waiter spun
   forever without re-reading the value; with no time slicing between equal priorities on a core, it starved
   the owner and nothing ran again. `unlock` cleared the owner before `arbitrate_unlock`, so a third thread
   could take the mutex while the kernel handed it to a waiter. Found on hardware: stopping a paused macro
   (the stop and the hold-expiry thread both waiting on the controller's lock) froze every client.
7. `input::Context` deactivates npad on drop only for its own applet resource user id, and never for id 0. It
   always deactivated id 0, which every non-applet process shares: Atmosphère's pm activates npad there once
   and reads held keys through it to decide title overrides, so dropping any `Context` (an NRO's input screen,
   a sysmodule's input reader) left pm seeing no buttons, and holding R no longer opened hbmenu, until a
   reboot. Found on hardware (fw 23.0.1).
8. `rrt0` stops the loader info string at its first NUL. hbloader passes its whole buffer (`nx-hbloader 2.4.5`,
   a NUL, then a quotation C callers never see, since libnx's `envGetLoaderInfo` is read as a C string), so
   `hbl::get_loader_info()` returned the quotation too.
