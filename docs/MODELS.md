# Supported Galaxy Buds models

The driver's INF matches the buds' A2DP sink service by Bluetooth Device ID:
`BTHENUM\{0000110b-0000-1000-8000-00805f9b34fb}_VID&00010075_PID&<pid>`
(vendor-ID source 0001 = Bluetooth SIG, vendor 0075 = Samsung). Windows cannot
match "any Samsung buds", so each product ID is listed.

The product ID does not say what a model can do. The service reads the buds'
SSC capability byte when it connects and picks the best mode they offer:
UHQ 96 kHz/24-bit, then Seamless HiFi 48 kHz/24-bit, then plain SSC 16-bit
(see `docs/DESIGN.md`).

| Model | Product ID | SSC capability byte | Best mode | Status |
|---|---|---|---|---|
| Galaxy Buds4 Pro (SM-R640) | `a013` | `0x3E` (measured) | UHQ 24/96 | **Tested**: HiFi and UHQ play correctly |
| Galaxy Buds3 Pro (SM-R630) | `a013` | `0x3E` (measured) | UHQ 24/96 | **Tested**: HiFi and UHQ play correctly |
| Galaxy Buds3 FE (SM-R420) | `a013` | `0x3C` (reported) | HiFi 24/48 | Untested |
| Galaxy Buds2 Pro (SM-R510) | `a013` | `0x3C` (reported) | HiFi 24/48 | Untested |
| Galaxy Buds3 (SM-R530) | `a013` | not reported | unknown | Untested |
| Galaxy Buds FE (SM-R400) | `a013` | not reported | unknown | Untested |
| Galaxy Buds2 (SM-R177) | `a013` | not reported | unknown | Untested |
| Galaxy Buds Pro (SM-R190) | `a013` | not reported | unknown | Untested |
| Galaxy Buds Live (SM-R180) | `a013` | not reported | unknown | Untested |
| Galaxy Buds Core | `a013` | not reported | unknown | Untested |
| Galaxy Buds+ (SM-R175) | `a012` | `0x38` (reported) | SSC 16-bit | Untested |
| Galaxy Buds (SM-R170) | `a011` | not reported | unknown | Untested |
| Galaxy Buds4 (SM-R540) | unknown | unknown | unknown | Not listed: no public report of its product ID. If it uses `a013` it is picked up automatically. |

"Tested" means on real hardware with this project. "Reported" values come
from public Linux and Windows device dumps (sources below), not from this
project. Models that share `a013` get the driver whether or not anyone has
confirmed their SSC endpoint.

**While this driver is installed it replaces Windows' own A2DP driver for
every paired device with a listed product ID.** If such a device turned out to
have no SSC endpoint, it would have no stereo audio until the driver is
uninstalled (hands-free audio is unaffected). Samsung lists the Scalable codec
for every model in the table.

Not listed on purpose: `a010` (Samsung U Flex) is shared with a speaker, and
other older Samsung/AKG headsets with the Scalable codec have no confirmed IDs.

## Sources for the product IDs

| Model | Source |
|---|---|
| Buds4 Pro, Buds3 Pro | Measured on the author's devices; also <https://github.com/maniacx/BudsLink/issues/155> |
| Buds3 FE | <https://linux-hardware.org/?probe=6a835ba7dc&log=input_devices> |
| Buds3 | <https://linux-hardware.org/?probe=194e707b15&log=input_devices> |
| Buds2 Pro | <https://github.com/lori28167/SamsungScalableCodec-linux/issues/1> |
| Buds FE | <https://linux-hardware.org/?probe=29d3373a2d&log=input_devices> |
| Buds2 | <https://github.com/bluez/bluez/issues/2640> |
| Buds Pro | <https://github.com/MichalW/gnome-bluetooth-battery-indicator/issues/29#issuecomment-1308751370> |
| Buds Live | <https://github.com/maniacx/Bluetooth-Battery-Meter/issues/67#issuecomment-3036543687> |
| Buds Core | <https://github.com/hmarquez-solutions/omarchy-buds/issues/1> |
| Buds+ | <https://github.com/arkq/bluez-alsa/issues/513>; capability byte: <https://github.com/EHfive/pulseaudio-modules-bt/issues/150> |
| Buds | <https://linux-hardware.org/?probe=5215a55d45&log=input_devices> |
