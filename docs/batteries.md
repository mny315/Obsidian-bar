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

The implementation sends only telemetry queries; it does not pair devices,
change mappings, disable controller emulation, or alter headset settings.

- [UPower device API](https://upower.freedesktop.org/docs/Device.html)
- [Solaar HID++ battery features](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/hidpp20.py)
- [Solaar Bolt receiver registers](https://github.com/pwr-Solaar/Solaar/blob/master/lib/logitech_receiver/receiver.py)
- [SDL Steam controller reports](https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/steam/controller_structs.h)
- [HyperHeadset Cloud III S protocol](https://github.com/LennardKittner/HyperHeadset/blob/main/src/devices/cloud_iii_s_wireless.rs)
