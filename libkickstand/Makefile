# Builds libkickstand.a with devkitA64: `make` (needs DEVKITPRO, default /opt/devkitpro).
DEVKITPRO ?= /opt/devkitpro
CC := $(DEVKITPRO)/devkitA64/bin/aarch64-none-elf-gcc
AR := $(DEVKITPRO)/devkitA64/bin/aarch64-none-elf-ar
CFLAGS := -march=armv8-a+crc+crypto -mtune=cortex-a57 -mtp=soft -fPIE -O2 -Wall -Wextra -Werror \
	-ffunction-sections -D__SWITCH__ -I$(DEVKITPRO)/libnx/include

libkickstand.a: kickstand.o
	$(AR) rcs $@ $^

kickstand.o: kickstand.c kickstand.h
	$(CC) $(CFLAGS) -c $< -o $@

clean:
	rm -f kickstand.o libkickstand.a

.PHONY: clean
