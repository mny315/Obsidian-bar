"""Native drag fixture for ignored UI tests. Requires one Niri output and uinput access."""
import fcntl
import json
import os
import struct
import subprocess
import sys
import time

outputs = json.loads(subprocess.check_output(["niri", "msg", "--json", "outputs"]))
active = [output["logical"] for output in outputs.values() if output.get("logical")]
assert len(active) == 1, "pointer tests require a single output"
width, height = active[0]["width"], active[0]["height"]
fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
for kind in (0, 1, 2, 3):  # EV_SYN, EV_KEY, EV_REL, EV_ABS
    fcntl.ioctl(fd, 0x40045564, kind)  # UI_SET_EVBIT
fcntl.ioctl(fd, 0x40045565, 272)  # UI_SET_KEYBIT: BTN_LEFT
fcntl.ioctl(fd, 0x40045566, 8)  # UI_SET_RELBIT: REL_WHEEL
for axis in (0, 1):
    fcntl.ioctl(fd, 0x40045567, axis)  # UI_SET_ABSBIT
maximum = [width, height] + [0] * 62
setup = struct.pack("80sHHHHI", b"Obsidian reorder test pointer", 3, 0x1234, 0x1234, 1, 0)
setup += struct.pack("64i", *maximum) + bytes(64 * 4 * 3)
os.write(fd, setup)
fcntl.ioctl(fd, 0x5501)  # UI_DEV_CREATE


def event(kind, code, value):
    os.write(fd, struct.pack("llHHi", 0, 0, kind, code, value))


def move(x, y):
    event(3, 0, int(x))
    event(3, 1, int(y))
    event(0, 0, 0)


x1, y1, x2, y2 = map(float, sys.argv[1:5])
wheel = int(sys.argv[5]) if len(sys.argv) > 5 else 0
try:
    time.sleep(0.4)
    move(x1, y1)
    time.sleep(0.02)
    move(x1, y1)
    event(1, 272, 1)
    event(0, 0, 0)
    time.sleep(0.03)
    if (x1, y1) != (x2, y2):
        for step in range(1, 31):
            move(x1 + (x2 - x1) * step / 30, y1 + (y2 - y1) * step / 30)
            time.sleep(0.025)
    for _ in range(abs(wheel)):
        event(2, 8, 1 if wheel > 0 else -1)
        event(0, 0, 0)
        time.sleep(0.08)
    if wheel:
        time.sleep(0.3)
    time.sleep(0.1)
    move(x2, y2)
    event(1, 272, 0)
    event(0, 0, 0)
    time.sleep(0.5)
finally:
    event(1, 272, 0)
    event(0, 0, 0)
    fcntl.ioctl(fd, 0x5502)  # UI_DEV_DESTROY
    os.close(fd)
