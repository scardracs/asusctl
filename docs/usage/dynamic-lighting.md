# Dynamic Lighting and legacy fallback

asusd prefers the Linux LED Dynamic Lighting ABI when a device exposes a valid
node below `/sys/class/leds`. A valid node must provide the generic `effect`,
`effect_index`, `zone_type`, and `led_count` attributes. Optional speed,
direction, palette, power-state, and direct-buffer controls are used only when
the node advertises them.

On supported ROG laptop keyboards, asusd retains the matching `hidraw` output
report as a capability-based fallback when no valid Dynamic Lighting node is
available. The two paths are mutually exclusive for a physical device:
Dynamic Lighting always wins, and hidraw is not used to retry an effect rejected
by the kernel. This preserves operation on released kernels while avoiding two
owners sending commands to the same controller.

The ASUS-only `aura_mode` attribute selects unified or split kernel topology; it
is not part of the generic Dynamic Lighting ABI. The current D-Bus API describes
historical keyboard and left/right lightbar subzones that the kernel topology
cannot represent accurately, so asusd does not advertise those zones through a
Dynamic Lighting controller. Whole-device requests use the unified node.

ROG NVMe enclosure lighting requires the kernel ASUS Aura SCSI Dynamic Lighting
driver. asusd matches an enclosure's block-device ancestry to its exact LED
node and briefly retries while that node is being registered. It never falls
back to the first enclosure. The old public `rog_scsi` SG_IO API was removed
intentionally: vendor commands are kernel-owned, and applications must use the
Dynamic Lighting sysfs ABI.

The optional generic `frame` attribute is not currently used by asusd. Direct
streaming uses `direct_buffer` and requires exactly `led_count * 3` RGB bytes.
Hardware-specific behavior still depends on the kernel driver reporting correct
topology, LED count, and optional attributes.
