# Peripheral batteries

The monitor's Battery section includes individual wireless devices. Each row can
be hidden or renamed in the existing monitor settings. Laptop batteries keep
their separate capacity-weighted total.

Sources:

- Linux power_supply device batteries and UPower (including Bluetooth devices).
- Logitech Logi Bolt receiver (046d:c548, HID++ interface 2), querying paired
  devices with Unified Battery (0x1004) or Battery Status (0x1000).
- Steam Controller Puck (28de:1304/1305), passively reading battery reports for
  each controller slot. The controller must be awake.
- HyperX Cloud III S Wireless (03f0:06be/02cc), requesting battery and charging
  state through its receiver. The headset must be switched on.

Receiver queries run in a background worker at most once every 15 seconds.
The first readings can take several seconds. Missing replies never become 0%.
Devices reporting only coarse levels show Low/Normal/Full instead of a made-up
percentage. Sleeping Logitech devices show “Asleep / unavailable”.

## Turning off a controller

Steam Controllers connected through a Puck have a power button beside their
battery reading. Clicking it sends the controller's power-off command to that
specific receiver slot in a background worker. Pairing is preserved; press the
controller's Steam button to switch it on again.

The button pauses while sending, then shows a check when the USB command has
been sent. This is not a wireless acknowledgement: the row disappears after a
subsequent battery poll (normally within about 15–20 seconds). If the controller
stays awake, the button becomes available again after 15 seconds. A warning icon
and tooltip explain failures and allow retry. Reconnecting the receiver is
supported even if its hidraw device number changes.

Only supported devices get a button. Logitech Bolt, HyperX, generic UPower and
Bluetooth battery rows currently remain telemetry-only; disconnecting Bluetooth
does not necessarily switch off a device.

## HyperX Cloud III S power-off investigation

As of 2026-10-16, no immediate headset power-off command has been verified for
the 03f0:06be/02cc receivers. The investigation covered HyperHeadset's Cloud III S
implementation and static inspection of the official NGENUITY 3.1.0 and Legacy
2.38.0.0 packages from [HyperX](https://hyperx.com/pages/ngenuity).

NGENUITY 3.1.0's Cloud3sWirelessProtocol exposes sleep-time configuration and
queries. Its AutoPowerOff settings offer 10, 20, or 30 minutes and Never. The
bundled Cloud3S Airoha library exposes Get/SetAutoPowerOffStatus, but no dedicated
immediate power-off operation was found. Legacy package strings likewise refer
to ChangeAutoPowerOff. These observations do not rule out an undocumented
firmware command.

HyperHeadset encodes the inactivity timeout with command 0x4a, as two big-endian
bytes of seconds, and reads it with 0x4b. Zero disables automatic shutdown; it
does not mean "turn off now". A short timeout would need hardware validation of
accepted values, idle detection, persistence, and restoration after reconnect
before it could serve as a reliable one-shot action. No headset settings were
changed during this investigation, and no speculative power-off button is
exposed in the monitor.

## HID permissions

The bar runs as the desktop user, without root. The package installs
`lib/udev/rules.d/70-obsidian-bar-batteries.rules`, granting access to supported
receiver interfaces only to the active local session via `uaccess`.

For NixOS, add the package to `services.udev.packages` as well as
`environment.systemPackages`, then rebuild:

```nix
services.udev.packages = [
  inputs.obsidian-bar.packages.${pkgs.stdenv.hostPlatform.system}.default
];
```

For other Linux distributions, install `assets/70-obsidian-bar-batteries.rules`
into `/etc/udev/rules.d/`, reload the rules, and reconnect the receivers. Never
run the whole bar as root. “Access needed” means the receiver permission rule
has not been applied.

## Protocol references

Automatic sampling sends only telemetry queries. The power button explicitly
sends Steam's `0x9f` command with the `off!` payload in Puck feature report `0x01`.
The implementation does not pair devices, change mappings, disable controller
emulation, or alter headset settings.

- [UPower device API](https://upower.freedesktop.org/docs/Device.html)
- [Solaar HID++ battery features](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp20.py)
- [Solaar Bolt receiver registers](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/receiver.py)
- [SDL Steam controller reports](https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/steam/controller_structs.h)
- [Puck per-slot power-off report](https://github.com/safijari/openpuck/blob/main/OpenPuck/puck_hid.cpp)
- [HyperHeadset Cloud III S protocol](https://github.com/LennardKittner/HyperHeadset/blob/main/src/devices/cloud_iii_s_wireless.rs)
