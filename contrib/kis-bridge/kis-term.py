#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# kis-term.py PTY LOG FIFO: hold a KIS console pty, copy everything it prints to LOG (raw),
# write whatever arrives on FIFO to the console. Auto: once U-Boot hands off to an EFI binary,
# send ESC every 0.15 s until GRUB draws its menu (interrupts a hidden-menu countdown); if
# U-Boot's prompt shows up instead, type 'boot'.
import os, sys, time, tty, re
pty, logf, fifo = sys.argv[1:4]
if not os.path.exists(fifo): os.mkfifo(fifo)
fd = os.open(pty, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK); tty.setraw(fd)
ff = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
log = open(logf, "ab", buffering=0)
buf = b""; esc = False; grub = False; booted_uboot = False; last = 0
def note(s): log.write(b"\n[kis-term %s] %s\n" % (time.strftime("%H:%M:%S").encode(), s.encode()))
while True:
    got = False
    try:
        d = os.read(fd, 65536)
        if d: log.write(d); buf = (buf + d)[-20000:]; got = True
    except BlockingIOError: pass
    except OSError as e: note("pty read error %s" % e); time.sleep(1)
    try:
        c = os.read(ff, 4096)
        if c: os.write(fd, c); got = True
    except BlockingIOError: pass
    if not grub:
        if not esc and re.search(rb"Booting /|bootaa64|grubaa64|EFI stub|Found EFI", buf):
            esc = True; note("EFI hand-off seen: ESC spam on")
        if re.search(rb"GNU GRUB|Use the .* keys|Press enter to boot|e to edit", buf):
            grub = True; esc = False; note("GRUB menu seen: ESC spam off")
        if not booted_uboot and re.search(rb"\n=> ?$", buf):
            booted_uboot = True; os.write(fd, b"boot\r"); note("U-Boot prompt: sent boot")
        if esc and time.time() - last > 0.15:
            os.write(fd, b"\x1b"); last = time.time()
    if not got: time.sleep(0.02)
