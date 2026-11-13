# Linker flags for libkickstand, for a devkitPro Makefile:
#   include /path/to/libkickstand/kickstand.mk
#   LDFLAGS += $(KICKSTAND_LDFLAGS)     (and add libkickstand's folder to LIBDIRS, or -L it)
#   LIBS    := -lkickstand $(LIBS)
KICKSTAND_WRAPS := appletMainLoop svcCreateThread svcCloseHandle svcMapMemory svcUnmapMemory \
	svcCreateCodeMemory svcControlCodeMemory svcCreateTransferMemory
KICKSTAND_LDFLAGS := -Wl,-u,kickstand_link $(foreach s,$(KICKSTAND_WRAPS),-Wl,--wrap=$(s))
