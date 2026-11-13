---
name: kickstand
description: How to drive a real Nintendo Switch through Kickstand's MCP tools (build, deploy, run, watch, play, debug homebrew). Use whenever the kickstand tools are available and the task touches the Switch, a homebrew NRO, a port tested on hardware, or the console's SD card.
---

# Working on a real Switch with Kickstand

The console is real hardware that someone owns, often in another room. Most mistakes cost the user a trip to
it, so look before you act and leave it how you found it.

## Start and finish

- Start with `status`: what's running, whether it's at the lock screen, keep-awake, whether USB is connected,
  and other agents. Don't ask the user to press anything before checking it yourself.
- If the console is unreachable, it's most likely asleep (it can't be woken over the network) or Kickstand
  isn't running. Say so and ask the user to wake it; don't retry in a loop.
- Every tool call keeps it awake for 30 minutes. Before a longer gap (a long build, waiting on the user), call
  `keep_awake`. When you're done, `keep_awake` with `minutes: 0` so it can sleep normally.
- If another agent is using the console (tools warn you), don't fight over it: wait, or ask the user.

## The dev loop

- A configured project (`projects`): `test_build` builds, checks the NRO matches its ELF, deploys, runs with
  full memory and supervises in one call. Prefer it.
- Iterating on homebrew that's already running: `reload_nro` swaps the new build in without the Home Menu.
  Its first line says which path it took (`hook`, `hbmenu` or `cold`) and where the time went (upload, exit,
  load). If the user's app keeps taking the slower `hbmenu` path, suggest the next-load hook (TOOLS.md
  "The next-load hook": a few lines of C or Rust before `main` returns) rather than adding it yourself
  unasked.
- Anything else: `run_nro`, then `nro_output` to read stdout. Use `grep`, `max_lines` and `timestamps` there
  instead of reading everything. `supervise` watches until exit, crash, fatal or hang.
- Games and ports need **full memory**, which is the default. `mode: "applet"` is only for small tools; never
  run a game in it.
- To stop a program, `quit_app` (it's asked to exit, as from the home menu, so it can save and flush its
  output). It ends hung programs too.
  `force_quit_app` kills it at once: only when `quit_app` says it didn't work.
- On a crash: `crash_reports`, then `crash_report` with the `elf`, which turns offsets into function names and
  source lines. For a hang: `threads` with the `elf` gives every thread's stack.
- Logs on the SD card: `tail_file` (it can follow and stop at a line), `grep` and `find`. Both run on the
  console, so nothing large crosses Wi-Fi.
- Sending files to the SD card: use `upload` for small files (configs, `.nro` builds), and prefer `usb_upload`
  for large files like ROMs, ISOs or `.o2r` asset archives whenever `status` reports a USB cable. It goes
  through Kickstand's own USB interface, so nothing on the console is opened or closed. If it says the PC
  needs a one-time setup (a udev rule, or a driver on Windows), pass that on to the user rather than trying
  to do it.

## Seeing the screen

- Screenshots cost tokens. Use `width: 640` (the default for most tools) and `region` to read small text at
  full resolution.
- Don't sleep and then screenshot. Use `wait_screen` (`still` for menus and fades, `change` for "did
  something happen"), or `then: "still"` on `press`, `input` and `play_macro`, which waits and looks in the
  same call.

## Input

- `press` for menus, `input` for timing (sticks, holds, touch, combos), and `input_until` to hold, mash or
  loop until the screen changes or a patch appears or goes ("hold right until the door appears").
- `type_text` types on a virtual keyboard: open the software keyboard first, and check it with a screenshot.
- **Macros** get you back to the same place in a game. `macros` lists what the user recorded, with notes
  saying where each one starts. `play_macro` from that place with `then: "still"`, then check where it ended
  and fix up with `press` or `input_until`: timing can drift over a long one.
- The user can pause or stop a macro while it plays (in the desktop app or a terminal). If `play_macro` says
  it was stopped and you didn't stop it, ask before playing it again. `control_macro` pauses, resumes or
  stops one; `status` shows one playing.
- To record a new one, the user has to be at the console: `record_macro` start, tell them exactly what to
  play and to put the controller down when done, then stop with a clear name and a note on where it starts.
- Or write one yourself: once a sequence of `input` steps reliably gets somewhere (checked on screen),
  `save_macro` the same steps with a note on where it starts, so the next run (or another agent) gets there in
  one call. Give loads generous waits.

## Safety

- Some SD card folders need the user's permission. If your app can't show Kickstand's question, Kickstand
  asks in its own window and tells you to ask in the chat; pass the user's answer to `answer_access`. Never
  guess an answer.
- Ask the user before deleting files, writing memory, or rebooting. `reboot` only does warm reboots into
  hekate (consoles injected over RCM must never be powered off), and it needs `confirm: true`.
- `write_memory` only works on homebrew. Don't try to work around a refusal; tell the user what was refused.
