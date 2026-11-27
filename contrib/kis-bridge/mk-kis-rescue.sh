#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Build the kis-rescue initramfs: static busybox + kis-rescue-init + the NVMe/reboot modules of ONE kernel.
#   mk-kis-rescue.sh MODULES_DIR BUSYBOX OUT.cpio.gz
# MODULES_DIR is lib/modules/<release> of the kernel you will boot (an unpacked modules tarball or a
# live /lib/modules/<release>); BUSYBOX is a static aarch64 busybox. Needs gen_init_cpio (from a
# kernel tree: usr/gen_init_cpio.c, 'cc -o gen_init_cpio usr/gen_init_cpio.c') on PATH or in GEN_INIT_CPIO.
set -euo pipefail
M=$(readlink -f "$1"); BB=$(readlink -f "$2"); OUT=$3
HERE=$(dirname "$(readlink -f "$0")"); GIC=${GEN_INIT_CPIO:-gen_init_cpio}
V=$(basename "$M"); [ -f "$M/modules.dep" ] || { echo "no modules.dep in $M" >&2; exit 1; }
MODS=${KIS_RESCUE_MODULES:-"nvme-apple macsmc-reboot"}
# The listed modules plus everything modules.dep says they need (built-ins are simply absent).
need=$(for m in $MODS; do grep -E "(^|/)$m\.ko(\.[a-z]+)?:" "$M/modules.dep" | tr -d ':'; done | tr ' ' '\n' | sort -u | grep .)
list=$(mktemp); trap 'rm -f "$list"' EXIT
{ echo "dir /dev 0755 0 0"; echo "nod /dev/console 0600 0 0 c 5 1"
  for d in /bin /proc /sys /run /tmp /sysroot /mnt /lib /lib/modules /lib/modules/$V; do echo "dir $d 0755 0 0"; done
  for k in $need; do p=""; for c in $(dirname "$k" | tr / ' '); do p="$p/$c"; echo "dir /lib/modules/$V$p 0755 0 0"; done; done | awk '!s[$0]++'
  echo "file /bin/busybox $BB 0755 0 0"; echo "slink /sbin bin 0777 0 0"; echo "slink /bin/sh busybox 0777 0 0"
  echo "file /init $HERE/kis-rescue-init 0755 0 0"
  for k in $need; do echo "file /lib/modules/$V/$k $M/$k 0644 0 0"; done; } > "$list"
"$GIC" "$list" | gzip -9 -n > "$OUT"
echo "$OUT: kernel $V, modules: $(echo $need | xargs -n1 basename | tr '\n' ' ')"
