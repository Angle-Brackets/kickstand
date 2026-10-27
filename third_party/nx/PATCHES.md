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
