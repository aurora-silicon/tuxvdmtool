# kis-bridge: KIS console and proxy for a tethered Mac, from a Linux host

A Linux host (an Apple silicon Mac running Linux, or any Linux box with a CD321x-style USB-C path to the
target) can now do what kisd on macOS did: reboot a tethered Mac over VDM into DebugUSB, read its console
over KIS, catch aura's proxy window and boot a kernel of your choice on it. No hands, no kernel driver.

Pieces:

| | |
|---|---|
| `tuxvdmtool` (this repo) | sends the VDMs (reboot, DebugUSB) |
| [kisd](https://github.com/AsahiLinux/kisd) (upstream, MIT, unmodified) | claims the target's Apple Debug USB interface (05ac:1881) with nusb and bridges one KIS UART channel to a pty |
| `kis-bridge` | runs kisd as a transient unit, links its pty at `/run/kis-<name>`, does the VDM sequence, catches the proxy window |
| `kis-catch.py` | sends proxy NOPs until aura (or m1n1) answers, drops the stale replies, prints `whoami`/chip id |
| `kis-rescue-init`, `mk-kis-rescue.sh` | a 1 MB rescue initramfs (static busybox + NVMe modules): a root shell on the console, or an unattended repair |
| `kis-term.py` | holds a KIS pty: logs it, writes FIFO input to it, sends ESC into a hidden GRUB menu (the GRUB route) |
| `kis-adt-node.py` | prints one ADT node through the proxy (used to find the target's DockChannel UART) |

## Install on the host

```sh
cargo install kisd                          # or build github.com/AsahiLinux/kisd for aarch64; 1.1.0 is tested
sudo install -m755 kisd tuxvdmtool /usr/local/bin/
# aura proxyclient (aura/proxyclient) with pyserial + construct importable
```

`kis-bridge` runs as root. kisd always links its pty at `/dev/m1n1`, so run one bridge per host. Give
proxyclient the `/run/kis-<name>` link, not the pty: aura's proxyclient switches to its KIS transfer mode
(bounded chunks with resync and retry) for paths named `kis-*` (and macOS's `cu.kis-*`).

## Rescue a Mac whose Linux does not boot

The target's aura Stage 1 must be a `DEBUGUSB_PROXY=1` build. With a host reading its DockChannel console on
a debugusb boot, Stage 1 prints `aura: debugusb boot, opening proxy window` and waits 15 s.

```sh
export AURA_PROXYCLIENT=/path/to/aura/proxyclient KIS_PYTHON=python3
sudo -E kis-bridge rescue j615 "USB-C Left Back" 30 3     # name, connector label on the HOST, catch s, attempts
# -> "proxy answered 1.20 s after start", "is_kis True whoami 1 chipid 0x8122"
```

Then boot the target's own kernel with the rescue initramfs (from its kernel package; same root as its
normal command line):

```sh
./mk-kis-rescue.sh <unpacked modules>/lib/modules/<release> busybox-static-aarch64 kis-rescue.cpio.gz
gzip -9 -k Image                          # linux.py takes .gz/.xz; aura unpacks it on the target
sudo AURADEVICE=/run/kis-j615 python3 $AURA_PROXYCLIENT/tools/linux.py \
  -b "root=UUID=<root uuid> ro rootflags=subvol=root console=tty0 console=ttyDC0 rescue.cmd=lib-symlink-fix" \
  --capture console.log --capture-seconds 300 Image.gz target.dtb kis-rescue.cpio.gz
```

`rescue.cmd=`:

- `shell` (default): a root shell on the console (no password: the installed system's locked root account
  does not matter, unlike dracut's `rd.break`).
- `mount`: the root mounted rw at `/sysroot`, then the shell.
- `lib-symlink-fix`: for usrmerge systems where a `tar -C /` without `--keep-directory-symlink` replaced
  `/lib` (or `/bin`, `/sbin`, `/lib64`) with a real directory. Entries the `/usr` side lacks are moved there,
  the directory is parked as `/lib.bad-<time>`, the symlink is restored, a log goes to
  `/var/log/kis-rescue-<time>.log`, then the Mac reboots. `rescue.noreboot` keeps the shell instead.

For an interactive shell over KIS the target DT needs a DockChannel UART node for the channel kisd reads
(`console=ttyDC0`). Find it from the ADT through the proxy: `AURADEVICE=/run/kis-<name> python3
kis-adt-node.py /arm-io/dockchannel-uart`. Channel n's config window is `reg[0] + n * 0x10000`, its data
window 0x4000 above that, the interrupt window is `reg[1]`; add `apple,channel = <n>` and, when the ADT
node has `enable-sw-drain`, `apple,sw-drain`. On the M3 Air (T8122) with kisd `--channel 1`:

```dts
serial@2e413c000 {
	compatible = "apple,dockchannel-uart";
	reg = <0x2 0xe413c000 0x0 0x4000>, <0x2 0xe4138000 0x0 0x4000>, <0x2 0xe410c000 0x0 0x1000>;
	reg-names = "data", "config", "irq";
	apple,channel = <1>;
	apple,sw-drain;
};
```

## Rescue through the Mac's own GRUB (no rescue kernel)

When the Mac's own Stage 2, U-Boot, GRUB and kernel are fine and only the root file system is damaged,
drive the normal boot chain over KIS instead of uploading a kernel. Proven on the M4 Air (J713) from an
M4 Pro host, 2026-11-26.

1. Catch the Stage 1 proxy over KIS channel 1 (`kis-bridge rescue`; release Stage 1 builds need the
   `aura-proxy` marker on the ESP for that window).
2. On the Stage 1 proxy: `nvme_init`, read the ANS SARTv3 entries, `nvme_shutdown`, and append
   `nvme.sart=<entries>` to the Mac's own `aura/stage2.bin` (as Stage 1's own chainload does; without it
   Stage 2's `nvme_init` data-aborts on a powered-off ANS).
3. Also append `chosen.aura,serial-console=1` and `chosen.stdout-path=<path of the apple,dockchannel-uart
   node>`. The Apple DTS sets `stdout-path = "serial0"`, which on M1-M4 laptops is the Samsung UART, so
   keeping it (serial-console=1 alone) leaves U-Boot on a UART nobody reads. With the DockChannel path,
   U-Boot (`CONFIG_APPLE_DOCKCHANNEL_SERIAL`) and then GRUB print on and read from KIS channel 0.
4. `chainload.py -r --vector-only stage2-with-vars.bin`, wait on channel 1 for Stage 2's
   `Vectoring to next stage`, then restart kisd on `--channel 0` (`kis-term.py` holds the pty, logs it,
   takes input from a FIFO, and sends ESC once U-Boot starts the EFI binary so a hidden GRUB menu stops).
5. In GRUB: `e` on the entry, Ctrl-N to the `linux` line, Ctrl-E, append ` console=ttyDC0 rdinit=/bin/sh`
   (initramfs shell) or ` console=ttyDC0 rw init=/bin/bash` (root file system shell, journal readable),
   Ctrl-X.

On an SELinux root, a file or symlink made from such a shell (or from the rescue initramfs) gets no label.
An unlabeled `/lib` symlink blocks every exec through `/lib/ld-linux-aarch64.so.1` once the system boots
Enforcing (dbus.socket start limit, gdm/upower dependency failures, agetty `can't exec /bin/login:
Permission denied`, no network). Copy the label of the matching `/usr` dir (`setfattr -h -n
security.selinux -v system_u:object_r:lib_t:s0 /lib` from `init=/bin/bash`) or `touch /.autorelabel`.
`lib-symlink-fix` now writes `/.autorelabel` when it changed anything on an SELinux root.

## What was proven, and the limits

M1 Pro (fleet89, CD321x on i2c) as host, M3 Air J615 as target, cable on the host's Left Back:

- VDM reboot + DebugUSB arm, kisd opens 05ac:1881, guessed base 0x2e4000000. Channel 1 carries aura.
- `kis-bridge rescue` caught the Stage 1 window 3 times out of 4 (1.2 s, 2.0 s and 1.5 s after the catch
  started). The miss: aura looks for a reader once, about 4 s into Stage 1, and when DebugUSB came up late
  (second arm) plus kisd's 1 s device poll, kisd drained the FIFO too late. The OS still kept the debug
  console. That is why `rescue` retries the whole sequence; the real fix is on the aura side (look for a
  reader for longer) and in kisd (poll for the device faster than once a second).
- `linux.py` over `/run/kis-j615`: a 21.8 MB Image.gz + DTB + 59 MB initramfs at about 130 KiB/s, 0 retries
  (2 of 2 uploads). The kernel console on ttyDC0 reached the host, and input from the host reached the target.
  A 16 KiB KIS write chunk ran at about 410 KiB/s once and hung in its resync retry the next time, so the
  1 KiB chunk stays.
- The rescue initramfs, unattended `lib-symlink-fix`, under QEMU (`-M virt -cpu max`) with the fleet
  kernel and a btrfs disk that has the same damage: moved `lib/modules/<ver>` into `/usr/lib/modules`,
  restored `/lib -> usr/lib`, left the other symlinks alone, wrote the log, rebooted.

Not working yet:

- The rescue initramfs booted on the J713 (kboot through the proxy, root shell on ttyDC0), but a proxy kboot
  leaves ANS powered off, so nvme-apple could not start it ('ANS did not boot'). The GRUB route above avoids
  that; the `.autorelabel` change has not been re-run under QEMU.
- `tuxvdmtool reboot debugusb` as one call failed on the M1 Pro -> J615 pair: the reboot VDM got no reply,
  and the retry path then armed DebugUSB on a target that had not rebooted. `kis-bridge vdm` sends `reboot`,
  then repeats `debugusb` until 05ac:1881 enumerates (up after arm 1 or 2, about 4 s).
- An M4 Pro host (SN201202x on SPMI, the kernel tipd interface) -> M4 Air: with the cable on the Air's rear
  port both VDMs are acknowledged but 05ac:1881 never enumerates (the host xHCI restarts with every HPM
  recovery). On the Air's front port (its DFU port) DebugUSB came up after 1-3 arms on every attempt (10+ boots), and the
  GRUB route above worked. Use the target's DFU port.
- When a failed VDM sends tuxvdmtool into its HPM recovery, every USB-C partner on that HPM's controller
  restarts. If the host is powered over the tether cable, check that it is on another supply first.
