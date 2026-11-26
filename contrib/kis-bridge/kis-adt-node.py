#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Print one ADT node (properties + translated reg) through the proxy: kis-adt-node.py /arm-io/dockchannel-uart
import sys, os
sys.path.insert(0, os.environ.get("AURA_PROXYCLIENT", "."))
from aura.setup import *  # noqa: F401,F403  (p, u, iface over AURADEVICE)
path = sys.argv[1] if len(sys.argv) > 1 else "/arm-io/dockchannel-uart"
node = u.adt[path]
for name in node._properties if hasattr(node, "_properties") else []:
    print(name, getattr(node, name, None))
print(node)
for i in range(8):
    try:
        print("reg[%d] = 0x%x +0x%x" % ((i,) + tuple(u.adt[path].get_reg(i))))
    except Exception as e:
        break
