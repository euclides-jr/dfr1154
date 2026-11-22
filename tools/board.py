#!/usr/bin/env python3
"""Exercise the DFR1154 over USB Serial/JTAG (Linux/macOS, Python stdlib only)."""
import argparse
import os
from pathlib import Path
import re
import select
import struct
import sys
import termios
import time
import tty
import zlib


def lines(port, command, timeout=15):
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    previous = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        settings = termios.tcgetattr(fd)
        settings[2] &= ~termios.HUPCL
        settings[4] = settings[5] = termios.B115200
        termios.tcsetattr(fd, termios.TCSANOW, settings)
        termios.tcflush(fd, termios.TCIFLUSH)
        os.write(fd, command)
        pending = b''
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], min(1, max(0, deadline - time.monotonic())))
            if not ready:
                continue
            chunk = os.read(fd, 8192)
            if not chunk:
                raise RuntimeError('Board disconnected')
            pending += chunk
            while b'\n' in pending:
                line, pending = pending.split(b'\n', 1)
                yield re.sub(r'\x1b\[[0-9;]*m', '', line.decode('ascii', errors='replace')).strip()
        raise TimeoutError('No complete response; check the port and close other serial monitors')
    finally:
        termios.tcsetattr(fd, termios.TCSANOW, previous)
        os.close(fd)


def checksum(data):
    value = 0x811c9dc5
    for byte in data:
        value = ((value ^ byte) * 0x01000193) & 0xffffffff
    return value


def save_png(path, width, height, data):
    """OV3660 RGB565, big-endian byte order on DVP."""
    rows = bytearray()
    for y in range(height):
        rows.append(0)  # PNG filter: none
        for x in range(width):
            offset = (y * width + x) * 2
            pixel = int.from_bytes(data[offset:offset + 2], 'big')
            r, g, b = (pixel >> 11) & 31, (pixel >> 5) & 63, pixel & 31
            rows.extend(((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)))

    def chunk(kind, payload):
        return struct.pack('>I', len(payload)) + kind + payload + struct.pack('>I', zlib.crc32(kind + payload))

    image = b'\x89PNG\r\n\x1a\n'
    image += chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
    image += chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b'')
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(image)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['frame', 'audio', 'tone', 'stats'])
    parser.add_argument('--port', default='/dev/ttyACM0')
    parser.add_argument('--output', type=Path, default=Path('captures/camera.png'))
    args = parser.parse_args()
    commands = {'frame': b'f', 'audio': b'a', 'tone': b't', 'stats': b'c'}
    response = lines(args.port, commands[args.command], timeout=60 if args.command == 'frame' else 15)
    try:
        data = bytearray()
        width = height = expected = 0
        for line in response:
            if 'ERROR' in line or 'panicked' in line:
                raise RuntimeError(line)
            if args.command != 'frame':
                print(line)
                if 'Speaker playback complete' in line or 'Camera captured' in line:
                    return
            elif line.startswith('FRAME '):
                _, w, h, fmt, size = line.split()
                width, height, expected = int(w), int(h), int(size)
                if fmt != 'RGB565_BE' or expected != width * height * 2 or not (0 < expected <= 1280 * 720 * 2):
                    raise RuntimeError('Unsupported frame format or size')
                data.clear()
            elif line.startswith('DATA ') and expected:
                data.extend(bytes.fromhex(line[5:]))
                if len(data) > expected:
                    raise RuntimeError('Frame exceeded announced size')
            elif line.startswith('END ') and expected:
                if len(data) != expected or checksum(data) != int(line[4:], 16):
                    raise RuntimeError('Incomplete frame or checksum mismatch')
                save_png(args.output, width, height, data)
                print(f'Saved {width}x{height} frame ({len(data)} bytes) to {args.output}')
                return
    finally:
        response.close()


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, ValueError, TimeoutError) as error:
        sys.exit(str(error))
