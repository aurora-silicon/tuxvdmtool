#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Catch aura's (or m1n1's) proxy window over a KIS bridge pty.

  kis-catch.py DEVICE [--seconds N] [--log FILE]

DEVICE is the bridge link (/run/kis-<name>); proxyclient treats a kis-* name as
a KIS link. The script sends proxy NOPs until the target's proxy answers (aura
opens a 15 s window on a debugusb boot), drops the stale NOP replies the retry
loop leaves queued, prints who answered and exits 0. Console text seen while
waiting is copied to --log. The target stays in its proxy for the next client
(linux.py, chainload.py, shell.py).

Needs aura's proxyclient on PYTHONPATH (or AURA_PROXYCLIENT=<dir>).
"""
import argparse
import os
import struct
import sys
import time

pc = os.environ.get("AURA_PROXYCLIENT")
if pc:
    sys.path.insert(0, pc)

from serial import Serial  # noqa: E402

REQ_NOP = 0x00AA55FF
CMD_LEN = 56


def checksum(data):
    s = 0xDEADBEEF
    for c in data:
        s = (s * 31337 + (c ^ 0x5A)) & 0xFFFFFFFF
    return (s ^ 0xADDEDBAD) & 0xFFFFFFFF


def nop_packet():
    cmd = struct.pack("<I", REQ_NOP) + b"\x00" * CMD_LEN
    return cmd + struct.pack("<I", checksum(cmd))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("device")
    ap.add_argument("--seconds", type=float, default=90)
    ap.add_argument("--log")
    a = ap.parse_args()

    log = open(a.log, "ab", buffering=0) if a.log else None
    deadline = time.time() + a.seconds
    while not os.path.exists(a.device):
        if time.time() > deadline:
            sys.exit(f"{a.device} did not appear")
        time.sleep(0.1)
    s = Serial(a.device, 115200, timeout=0)
    pkt = nop_packet()
    # A reply is 36 bytes: type (= the request), status, payload, checksum over the first 32.
    want = struct.pack("<I", REQ_NOP)
    buf = b""
    t0 = time.time()
    caught = None
    while time.time() < deadline:
        s.write(pkt)
        end = time.time() + 0.05
        while time.time() < end:
            d = s.read(4096)
            if d:
                buf += d
                if log:
                    log.write(d)
        i = buf.find(want)
        while i >= 0 and len(buf) - i >= 36:
            r = buf[i:i + 36]
            if struct.unpack("<I", r[32:])[0] == checksum(r[:32]):
                caught = time.time() - t0
                break
            i = buf.find(want, i + 1)
        if caught is not None:
            break
        buf = buf[-4096:]
    if caught is None:
        print(f"no proxy reply within {a.seconds:.0f} s", flush=True)
        sys.exit(1)
    print(f"proxy answered {caught:.2f} s after start", flush=True)

    # Drain the replies to the NOPs still in flight, then check with a clean exchange.
    time.sleep(0.5)
    stale = 0
    while True:
        d = s.read(65536)
        if not d:
            time.sleep(0.2)
            d = s.read(65536)
            if not d:
                break
        stale += len(d)
    print(f"drained {stale} stale bytes", flush=True)
    s.close()

    from aura.proxy import UartInterface, AuraProxy  # noqa: E402
    iface = UartInterface(a.device)
    p = AuraProxy(iface)
    iface.nop()
    print(f"is_kis {iface.is_kis} whoami {p.iodev_whoami()} chipid 0x{p.get_chipid():x} "
          f"base 0x{p.get_base():x}", flush=True)


if __name__ == "__main__":
    main()
