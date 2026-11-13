// SPDX-License-Identifier: GPL-3.0-only
// libkickstand: link it into a libnx homebrew app so Kickstand's `reload_nro` can swap in a new build in the
// same hbloader process, safely. Three parts, none of which needs a call from the app:
//
// 1. Quit on request. `appletMainLoop()` returns false once Kickstand asks for a reload (it leaves
//    sdmc:/kickstand/next-load), exactly as when the Home menu asks the app to close; the app then quits
//    the way it always does. No button press, and apps that only quit from a menu can be reloaded.
// 2. The next-load hook. On the way out the request is claimed (deleted) and handed to hbloader with
//    envSetNextLoad, so hbloader loads the new build instead of hbmenu.
// 3. Cleanup. hbloader loads the next NRO into this same process and gives it the same heap. libnx doesn't
//    undo what an app leaves behind: a thread that was never closed keeps its stack mapped (the heap pages
//    under it stay inaccessible), and JIT code memory keeps its heap pages too. The next NRO's malloc then
//    runs into those holes and crashes, and a thread still blocked in the old code jumps into whatever
//    loads there next. The kernel calls that create these are wrapped (`--wrap`) to keep a list, and at exit
//    the leftovers are released: threads get a moment to finish, those still running are paused for good,
//    and every mapping still outstanding is undone.
//
// Parts 1 and 2 only act when the app was started from a PC over nxlink (`__nxlink_host` is set), so a
// launch from hbmenu by hand behaves as before. Part 3 always runs: it also makes quitting to hbmenu and
// starting another NRO safe.
//
// Linking: see kickstand.h.

#include <switch.h>
#include <switch/services/bsd.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

#include "kickstand.h"

#define REQUEST "sdmc:/kickstand/next-load"
// Kickstand looks for this in an NRO to know it can ask the app to quit instead of pressing buttons.
const char kickstand_marker[] = "kickstand-lib/1 " REQUEST;

// Referenced with -Wl,-u,kickstand_link so the linker always takes this object.
void kickstand_link(void) {}

// ---------------------------------------------------------------------------------------------------------
// Tracking. Fixed tables: no malloc here (a paused thread could be holding malloc's lock at exit).

#define MAX_THREADS 256
#define MAX_MAPS 256
#define MAX_CODE 16
#define MAX_TMEM 64

typedef struct {
    Handle handle;
    void* stack_top;
} TrackedThread;
typedef struct {
    void* dst;
    void* src;
    u64 size;
} TrackedMap;
typedef struct {
    Handle handle;
    void* owner;
    u64 owner_size;
    void* slave;
    u64 slave_size;
} TrackedCode;

static TrackedThread g_threads[MAX_THREADS];
static TrackedMap g_maps[MAX_MAPS];
static TrackedCode g_code[MAX_CODE];
static Handle g_tmem[MAX_TMEM];
static bool g_overflow;
static u32 g_lock; // 0 free, 1 held: a spin lock, so nothing here waits in the kernel

static void Lock(void) {
    while (__atomic_exchange_n(&g_lock, 1, __ATOMIC_ACQUIRE)) {
        svcSleepThread(0);
    }
}
static void Unlock(void) {
    __atomic_store_n(&g_lock, 0, __ATOMIC_RELEASE);
}
// At exit: don't wait forever on a lock a paused or stuck thread may hold.
static bool LockForExit(void) {
    for (int i = 0; i < 1000; i++) {
        if (!__atomic_exchange_n(&g_lock, 1, __ATOMIC_ACQUIRE)) return true;
        svcSleepThread(100000);
    }
    return false;
}

Result __real_svcCreateThread(Handle* out, void* entry, void* arg, void* stack_top, int prio, int cpuid);
Result __wrap_svcCreateThread(Handle* out, void* entry, void* arg, void* stack_top, int prio, int cpuid) {
    Result rc = __real_svcCreateThread(out, entry, arg, stack_top, prio, cpuid);
    if (R_SUCCEEDED(rc)) {
        Lock();
        int i = 0;
        while (i < MAX_THREADS && g_threads[i].handle) i++;
        if (i < MAX_THREADS) g_threads[i] = (TrackedThread){ *out, stack_top };
        else g_overflow = true;
        Unlock();
    }
    return rc;
}

Result __real_svcCreateTransferMemory(Handle* out, void* addr, size_t size, u32 perm);
Result __wrap_svcCreateTransferMemory(Handle* out, void* addr, size_t size, u32 perm) {
    Result rc = __real_svcCreateTransferMemory(out, addr, size, perm);
    if (R_SUCCEEDED(rc)) {
        Lock();
        int i = 0;
        while (i < MAX_TMEM && g_tmem[i]) i++;
        if (i < MAX_TMEM) g_tmem[i] = *out;
        else g_overflow = true;
        Unlock();
    }
    return rc;
}

Result __real_svcCreateCodeMemory(Handle* code_handle, void* src_addr, u64 size);
Result __wrap_svcCreateCodeMemory(Handle* code_handle, void* src_addr, u64 size) {
    Result rc = __real_svcCreateCodeMemory(code_handle, src_addr, size);
    if (R_SUCCEEDED(rc)) {
        Lock();
        int i = 0;
        while (i < MAX_CODE && g_code[i].handle) i++;
        if (i < MAX_CODE) g_code[i] = (TrackedCode){ .handle = *code_handle };
        else g_overflow = true;
        Unlock();
    }
    return rc;
}

Result __real_svcControlCodeMemory(Handle code_handle, CodeMapOperation op, void* dst_addr, u64 size, u64 perm);
Result __wrap_svcControlCodeMemory(Handle code_handle, CodeMapOperation op, void* dst_addr, u64 size, u64 perm) {
    Result rc = __real_svcControlCodeMemory(code_handle, op, dst_addr, size, perm);
    if (R_SUCCEEDED(rc)) {
        Lock();
        for (int i = 0; i < MAX_CODE; i++) {
            TrackedCode* c = &g_code[i];
            if (c->handle != code_handle) continue;
            switch (op) {
            case CodeMapOperation_MapOwner: c->owner = dst_addr; c->owner_size = size; break;
            case CodeMapOperation_MapSlave: c->slave = dst_addr; c->slave_size = size; break;
            case CodeMapOperation_UnmapOwner: c->owner = NULL; break;
            case CodeMapOperation_UnmapSlave: c->slave = NULL; break;
            }
        }
        Unlock();
    }
    return rc;
}

Result __real_svcMapMemory(void* dst_addr, void* src_addr, u64 size);
Result __wrap_svcMapMemory(void* dst_addr, void* src_addr, u64 size) {
    Result rc = __real_svcMapMemory(dst_addr, src_addr, size);
    if (R_SUCCEEDED(rc)) {
        Lock();
        int i = 0;
        while (i < MAX_MAPS && g_maps[i].size) i++;
        if (i < MAX_MAPS) g_maps[i] = (TrackedMap){ dst_addr, src_addr, size };
        else g_overflow = true;
        Unlock();
    }
    return rc;
}

Result __real_svcUnmapMemory(void* dst_addr, void* src_addr, u64 size);
Result __wrap_svcUnmapMemory(void* dst_addr, void* src_addr, u64 size) {
    Result rc = __real_svcUnmapMemory(dst_addr, src_addr, size);
    if (R_SUCCEEDED(rc)) {
        Lock();
        for (int i = 0; i < MAX_MAPS; i++) {
            if (g_maps[i].size && g_maps[i].dst == dst_addr) g_maps[i] = (TrackedMap){ 0 };
        }
        Unlock();
    }
    return rc;
}

Result __real_svcCloseHandle(Handle handle);
Result __wrap_svcCloseHandle(Handle handle) {
    // Forgotten before it's closed: once closed, the number can be handed out again at once.
    Lock();
    for (int i = 0; i < MAX_THREADS; i++) {
        if (g_threads[i].handle == handle) g_threads[i].handle = 0;
    }
    for (int i = 0; i < MAX_TMEM; i++) {
        if (g_tmem[i] == handle) g_tmem[i] = 0;
    }
    for (int i = 0; i < MAX_CODE; i++) {
        if (g_code[i].handle == handle) g_code[i].handle = 0;
    }
    Unlock();
    return __real_svcCloseHandle(handle);
}

// ---------------------------------------------------------------------------------------------------------
// Quit on request.

// To the PC over nxlink. write(), not printf: some apps send stdout elsewhere or never flush it, and at exit
// a paused thread may hold stdout's lock.
static void Say(const char* text) {
    // Not once the app has called socketExit(): stdout is still the nxlink socket, and writing to it then
    // crashes in newlib (Uprising NX does this before returning from main).
    if (!serviceIsActive(bsdGetServiceSession())) return;
    write(STDOUT_FILENO, text, strlen(text));
}

static bool FromPc(void) {
    return __nxlink_host.s_addr != 0;
}

bool kickstand_quit_requested(void) {
    static bool requested;
    static u64 next_check;
    if (requested) return true;
    if (!FromPc()) return false;
    const u64 now = armGetSystemTick();
    if (now < next_check) return false;
    next_check = now + armGetSystemTickFreq() / 5; // a stat 5 times a second, about 0.2 ms each
    struct stat st;
    if (stat(REQUEST, &st) == 0) {
        requested = true;
        Say("kickstand: reload requested, quitting\n");
    }
    return requested;
}

bool __real_appletMainLoop(void);
bool __wrap_appletMainLoop(void) {
    if (kickstand_quit_requested()) return false;
    return __real_appletMainLoop();
}

// ---------------------------------------------------------------------------------------------------------
// At exit.

// Hands Kickstand's request to hbloader, if there is one and this run came from a PC. Deleting the file
// claims it: Kickstand withdraws a request it gave up on by deleting it too, so whichever delete works
// decides.
static void ClaimNextLoad(void) {
    if (!FromPc()) return;
    FILE* f = fopen(REQUEST, "r");
    if (!f) return;
    char path[512], argv[2048];
    const bool ok = fgets(path, sizeof(path), f) && fgets(argv, sizeof(argv), f);
    fclose(f);
    if (!ok) return;
    path[strcspn(path, "\r\n")] = 0;
    argv[strcspn(argv, "\r\n")] = 0;
    if (unlink(REQUEST) == 0) {
        envSetNextLoad(path, argv);
        char line[600];
        snprintf(line, sizeof(line), "kickstand: next load %s\n", path);
        Say(line);
    }
}

// What the cleanup did, item by item, for sdmc:/kickstand/libkickstand.log (stdout gets the summary; an app
// may have closed its sockets before exit).
static char g_detail[8192];
static size_t g_detail_len;
__attribute__((format(printf, 1, 2))) static void Detail(const char* fmt, ...) {
    if (g_detail_len >= sizeof(g_detail) - 1) return;
    va_list ap;
    va_start(ap, fmt);
    const int n = vsnprintf(g_detail + g_detail_len, sizeof(g_detail) - g_detail_len, fmt, ap);
    va_end(ap);
    if (n > 0) g_detail_len += (size_t)n < sizeof(g_detail) - g_detail_len ? (size_t)n : sizeof(g_detail) - 1 - g_detail_len;
}

static bool Contains(void* base, u64 size, const void* addr) {
    return (const u8*)addr >= (const u8*)base && (const u8*)addr < (const u8*)base + size;
}

// Releases what the app left behind. Returns a line to print, or "" when there was nothing.
static void Cleanup(char* report, size_t report_size) {
    report[0] = 0;
    if (!LockForExit()) {
        snprintf(report, report_size, "kickstand: cleanup skipped (a thread holds the tracking lock)\n");
        return;
    }
    u64 self = 0;
    svcGetThreadId(&self, CUR_THREAD_HANDLE);
    const void* sp = __builtin_frame_address(0);

    // Threads: a moment to finish (1 s in all), then paused for good. The kernel can't end another thread.
    int exited = 0, paused = 0, stuck = 0;
    bool running[MAX_THREADS] = { 0 };
    const u64 deadline = armGetSystemTick() + armGetSystemTickFreq();
    for (int i = 0; i < MAX_THREADS; i++) {
        TrackedThread* t = &g_threads[i];
        if (!t->handle) continue;
        u64 tid = 0;
        if (R_SUCCEEDED(svcGetThreadId(&tid, t->handle)) && tid == self) {
            t->handle = 0; // exit() on a thread of our own: leave it be
            continue;
        }
        const u64 now = armGetSystemTick();
        const u64 left_ns = now < deadline ? armTicksToNs(deadline - now) : 0;
        Result rc = 0;
        if (R_SUCCEEDED(svcWaitSynchronizationSingle(t->handle, left_ns))) {
            exited++;
            Detail("thread %llx (handle %x, stack top %p): finished\n", (unsigned long long)tid, t->handle, t->stack_top);
        } else if (R_SUCCEEDED(rc = svcSetThreadActivity(t->handle, ThreadActivity_Paused))) {
            paused++;
            Detail("thread %llx (handle %x, stack top %p): still running, paused\n", (unsigned long long)tid, t->handle, t->stack_top);
        } else {
            running[i] = true; // can't stop it: leave its stack alone
            stuck++;
            Detail("thread %llx (handle %x, stack top %p): still running, pausing failed (%x)\n", (unsigned long long)tid, t->handle, t->stack_top, rc);
        }
    }

    // JIT code memory: both views unmapped, then the handle closed (the heap pages come back with it).
    int code = 0;
    for (int i = 0; i < MAX_CODE; i++) {
        TrackedCode* c = &g_code[i];
        if (!c->handle) continue;
        const Result slave = c->slave ? __real_svcControlCodeMemory(c->handle, CodeMapOperation_UnmapSlave, c->slave, c->slave_size, 0) : 0;
        const Result owner = c->owner ? __real_svcControlCodeMemory(c->handle, CodeMapOperation_UnmapOwner, c->owner, c->owner_size, 0) : 0;
        const Result closed = __real_svcCloseHandle(c->handle);
        Detail("code memory %x: slave %p (%x), owner %p (%x), %llu KiB, closed (%x)\n", c->handle, c->slave, slave, c->owner, owner,
               (unsigned long long)(c->owner_size / 1024), closed);
        *c = (TrackedCode){ 0 };
        code++;
    }

    // Stacks and other aliases: unmapped, except this thread's and those of threads that couldn't be stopped.
    int maps = 0;
    u64 bytes = 0;
    for (int i = 0; i < MAX_MAPS; i++) {
        TrackedMap* m = &g_maps[i];
        if (!m->size || Contains(m->dst, m->size, sp)) continue;
        bool in_use = false;
        for (int j = 0; j < MAX_THREADS; j++) {
            if (running[j] && Contains(m->dst, m->size, (const u8*)g_threads[j].stack_top - 1)) in_use = true;
        }
        if (in_use) {
            Detail("mapping %p <- %p, %llu KiB: kept (a thread still uses it)\n", m->dst, m->src, (unsigned long long)(m->size / 1024));
            continue;
        }
        const Result rc = __real_svcUnmapMemory(m->dst, m->src, m->size);
        Detail("mapping %p <- %p, %llu KiB: unmapped (%x)\n", m->dst, m->src, (unsigned long long)(m->size / 1024), rc);
        if (R_SUCCEEDED(rc)) {
            maps++;
            bytes += m->size;
        }
        *m = (TrackedMap){ 0 };
    }

    // Handles: finished and paused threads (a paused thread stays, without its stack), transfer memory.
    for (int i = 0; i < MAX_THREADS; i++) {
        if (g_threads[i].handle && !running[i]) __real_svcCloseHandle(g_threads[i].handle);
        g_threads[i].handle = 0;
    }
    int tmem = 0;
    for (int i = 0; i < MAX_TMEM; i++) {
        if (g_tmem[i]) {
            Detail("transfer memory %x: closed (%x)\n", g_tmem[i], __real_svcCloseHandle(g_tmem[i]));
            g_tmem[i] = 0;
            tmem++;
        }
    }
    Unlock();

    if (exited || paused || stuck || code || maps || tmem || g_overflow) {
        snprintf(report, report_size,
                 "kickstand: cleaned up after the app: %d finished thread(s) closed, %d still running paused%s, "
                 "%d JIT code memory, %d mapping(s) (%llu KiB) unmapped, %d transfer memory closed%s\n",
                 exited, paused, stuck ? " (some couldn't be paused: their stacks stay)" : "", code, maps,
                 (unsigned long long)(bytes / 1024), tmem, g_overflow ? " (some weren't tracked: tables full)" : "");
    }
}

static void KickstandExit(void) {
    // A real reference, so --gc-sections keeps the marker Kickstand looks for.
    __asm__ volatile("" ::"r"(kickstand_marker));
    ClaimNextLoad();
    char report[384];
    Cleanup(report, sizeof(report));
    if (report[0] && FromPc()) {
        // open/write, not stdio: a paused thread may hold a stdio lock.
        const int fd = open("sdmc:/kickstand/libkickstand.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
        if (fd >= 0) {
            write(fd, report, strlen(report));
            write(fd, g_detail, g_detail_len);
            close(fd);
        }
    }
    if (report[0]) Say(report);
}

// libnx never runs .fini_array (no destructor attribute works), but exit() runs atexit handlers, last
// registered first. Registered before any other constructor (101 is the first priority programs may use),
// this runs after everything the app registers, its C++ static destructors included.
__attribute__((constructor(101))) static void KickstandInit(void) {
    atexit(KickstandExit);
}
