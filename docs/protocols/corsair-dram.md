# Corsair DRAM Protocol

SMBus protocol for Corsair Vengeance and Dominator DDR4/DDR5 DRAM RGB controllers.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) (GPL-2.0-or-later): `CorsairVengeanceController`.

**Source:** `src/daemon/src/drivers/vendors/corsair/protocols/corsair_dram.rs`

---

## Overview

Corsair DRAM sticks expose an RGB controller on the chipset SMBus. HaloDaemon probes addresses `0x58`–`0x5F` and `0x18`–`0x1F` (16 addresses total) via the [SMBus transport](../transports/smbus.md).

The controller uses a 32-byte device info block with CRC8 protection, followed by raw per-LED color writes.

---

## Device detection

HaloDaemon identifies Corsair DRAM by reading the 32-byte info block from the device and checking for a valid CRC8. The info block includes device type and LED count fields.

---

## Color write

LED colors are written as raw RGB bytes directly to the controller. The write address and protocol differ between DDR4 and DDR5 variants; the driver selects the correct variant from the device info block's device type field.

---

## Limitations

- Only Vengeance and Dominator DDR4 are tested; other Corsair DRAM may use different addresses or protocol variants.
- Requires the `i2c` group on Linux and PawnIO + Administrator on Windows (see [SMBus transport](../transports/smbus.md)).
