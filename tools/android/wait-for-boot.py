#!/usr/bin/env python3
"""Wait for an emulator's observable boot state, with a bounded deadline."""
import subprocess
import sys
import time

serial = sys.argv[1]
deadline = time.monotonic() + 180
while time.monotonic() < deadline:
    try:
        result = subprocess.run(
            ["adb", "-s", serial, "shell", "getprop", "sys.boot_completed"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip() == "1":
            print("Android boot completed")
            break
    except subprocess.TimeoutExpired:
        pass
    time.sleep(0.5)
else:
    raise SystemExit(f"{serial} did not boot within 180 seconds; check adb devices")
