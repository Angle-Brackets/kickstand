// SPDX-License-Identifier: GPL-3.0-only
// libkickstand: fast, safe hot reload for libnx homebrew under Kickstand (see kickstand.c for what it does).
//
// Nothing to call. Link it, with these flags (before the libraries, so the linker takes the object):
//
//   -Wl,-u,kickstand_link -Wl,--wrap=appletMainLoop -Wl,--wrap=svcCreateThread -Wl,--wrap=svcCloseHandle
//   -Wl,--wrap=svcMapMemory -Wl,--wrap=svcUnmapMemory -Wl,--wrap=svcCreateCodeMemory
//   -Wl,--wrap=svcControlCodeMemory -Wl,--wrap=svcCreateTransferMemory -lkickstand
//
// (kickstand.mk has them as KICKSTAND_LDFLAGS; or compile kickstand.c into the app and use the same flags
// without -lkickstand.)
//
// An app whose main loop doesn't call appletMainLoop() can call kickstand_quit_requested() once a frame
// instead, and quit when it returns true.
#pragma once

#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/// Whether Kickstand asked this app to quit for a reload (checked at most 5 times a second; true from
/// then on). Always false unless the app was started from a PC over nxlink.
bool kickstand_quit_requested(void);

#ifdef __cplusplus
}
#endif
