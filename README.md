# DFRobot DFR1154

Rust `no_std` firmware for the [DFRobot ESP32-S3 AI Camera Module](https://wiki.dfrobot.com/dfr1154), created with `esp-generate` 2.0.0 and using `esp-hal` 1.2.2.

The firmware initializes the OV3660 camera, PDM microphone and MAX98357 speaker
amplifier. At startup it plays a quiet 250 ms test tone, captures one camera
frame, and starts listening for USB commands. Audio recording is triggered by
BOOT or a USB command. The camera uses two PSRAM frame buffers, and audio buffers use internal RAM.

## Build and flash

The Xtensa `esp` Rust toolchain and `espflash` must be installed. Load the
environment installed by `espup`, then build and flash:

```sh
source ~/export-esp.sh
cargo build --release
cargo run --release -- --port /dev/ttyACM0 --non-interactive
```

If automatic download mode fails, hold BOOT, press and release RST, then
release BOOT and retry. Exit the monitor with Ctrl+C.

The serial port must be writable by your user. The runner configures the
ESP32-S3 target and 16 MB flash.

## Wi-Fi camera viewer

Set `WIFI_SSID` and `WIFI_PASSWORD` in the build environment for your 2.4 GHz
WPA2-Personal network, then flash with the command above. For example, in Bash:

```sh
read -r -p "Wi-Fi SSID: " WIFI_SSID
read -r -s -p "Wi-Fi password: " WIFI_PASSWORD
echo
export WIFI_SSID WIFI_PASSWORD
cargo run --release -- --port /dev/ttyACM0 --non-interactive
unset WIFI_SSID WIFI_PASSWORD
```

Once the board joins Wi-Fi and gets an IPv4 address via DHCP, the USB monitor
prints `Camera viewer: http://<address>/`. Open that URL on a device on the same
network. Devices with multicast DNS support can also open
`http://dfrobot.local/`. The viewer displays a 1280×720 MJPEG feed from `/stream` over one HTTP
connection. The camera can capture the next frame into its second PSRAM buffer
while Wi-Fi sends the current frame. The board serves one stream at a time. Close the stream to use
`/frame` for individual JPEG snapshots.
The OV3660 compresses frames on the sensor. USB captures still use RGB565.
The HTTP feed is unencrypted and accessible to anyone
who can reach the board. The credentials are embedded in the firmware image.
Without both build variables, the firmware runs in USB-only mode.

## Sendspin playback

For a Wi-Fi-enabled build, set `SENDSPIN_SERVER_IP` to the Music Assistant
server's IPv4 address. Provision one private identity key per board and reuse
it on subsequent builds. For example, run these commands in Bash before building:

```bash
install -d -m 700 ~/.config/dfr1154
if [[ ! -s ~/.config/dfr1154/sendspin-identity ]]; then
    (umask 077 && python3 -c 'import secrets; print(secrets.token_urlsafe(32))' > ~/.config/dfr1154/sendspin-identity)
fi
read -r SENDSPIN_IDENTITY_KEY < ~/.config/dfr1154/sendspin-identity
export SENDSPIN_IDENTITY_KEY
```

The key is embedded in the firmware. Keep both the key and firmware image
private. For multiple boards, give each its own key file.

The firmware connects on port 8927 and advertises 48 kHz, 16-bit stereo PCM.
Music Assistant must authorize guest playback for this identity. Select
**DFRobot** after the serial log reports `Sendspin player ready`. The player
starts at 25% volume and accepts volume and mute commands from the server.

PCM is queued in a 384 KiB PSRAM buffer and fed to a 16 KiB I²S DMA ring.
Local playback starts at the timestamp estimated by the clock filter. The current player supports PCM
only. Pairing, microphone streaming, and continuous sample-rate correction
during playback are still pending.

## Live camera and microphone over WebRTC

The ESP32 captures 16 kHz mono PCM continuously while a gateway is listening.
To enable the mic endpoint, generate a separate token and export it before
building and flashing the firmware:

```bash
install -d -m 700 ~/.config/dfr1154
if [[ ! -s ~/.config/dfr1154/mic-token ]]; then
    (umask 077 && python3 -c 'import secrets; print(secrets.token_urlsafe(32))' > ~/.config/dfr1154/mic-token)
fi
read -r MIC_STREAM_TOKEN < ~/.config/dfr1154/mic-token
export MIC_STREAM_TOKEN
```

On the NanoPi, copy `tools/mic_gateway/` and set `MIC_STREAM_TOKEN` to the same
value. Install the Python dependencies and run the gateway with a certificate
trusted by your phone:

```sh
python3 -m pip install -r requirements.txt
python3 gateway.py --board dfrobot.local --cert /path/to/fullchain.pem --key /path/to/privkey.pem
```

Open the gateway and tap **Start monitor**. The NanoPi decodes the ESP32's
MJPEG stream and encodes it for the browser alongside the mic in one WebRTC
session. Video is capped at 8 fps by default. Use `--video-fps` to change the
cap, or set `VIDEO_FPS` in the Docker Compose environment. One browser can monitor at a time, and the gateway occupies the ESP32's
single camera stream while the monitor is active.

Anyone who can reach the browser endpoint can listen and watch, so keep it on
your LAN. The mic TCP link on port 8096 is token-protected and unencrypted.
Use the board's IP address for `--board` if the NanoPi cannot resolve `.local`
names. Set `MIC_GAIN` on the gateway to adjust microphone gain from 1× to 8×.
The default is 4×, with a soft limiter on loud peaks.

Pass `--hardware-video` on a gateway with a working V4L2 H.264 encoder.
The container needs access to the encoder device. The gateway uses hardware
when H.264 is negotiated and falls back to software if the encoder fails.

## Camera and audio commands

Close the serial monitor before using the helper. It requires Python 3 on
Linux/macOS and has no third-party Python dependencies:

```sh
# Capture a checksum-verified 1280x720 RGB565 frame and save a PNG.
python tools/board.py frame --output captures/camera.png

# Record 500 ms at 16 kHz / 16-bit mono, then replay at half amplitude.
python tools/board.py audio

# Play a quiet 440 Hz test tone, independent of the microphone.
python tools/board.py tone

# Capture a frame and print its byte count and checksum.
python tools/board.py stats
```

Use `--port /dev/ttyACM1` to select another port. Pressing BOOT also records and
replays audio; the LED stays on during that operation. Connect a compatible
speaker to the SPK connector to hear the output.

In an interactive serial monitor, send `a` for record/play, `t` for tone,
`c` for camera statistics, or `f` for a hex frame dump. The frame protocol is
`FRAME <width> <height> RGB565_BE <bytes>`, followed by `DATA <hex>` lines and
`END <FNV-1a checksum>`. The helper validates the length and checksum before
saving the PNG. Recording, USB frame downloads, and HTTP frame requests share the main loop.

## Board wiring

| Function | Pins / peripheral |
| --- | --- |
| Microphone PDM clock / data | GPIO38 / GPIO39; I2S0, DMA_CH0 |
| Speaker BCLK / LRCLK / data | GPIO45 / GPIO46 / GPIO42; I2S1, DMA_CH1 |
| Camera SCCB SDA / SCL | GPIO8 / GPIO9; I2C0, address 0x3c |
| Camera XCLK / PCLK | GPIO5 / GPIO15; 20 MHz XCLK |
| Camera VSYNC / HREF | GPIO1 / GPIO2 |
| Camera D0–D7 | GPIO16, 18, 21, 17, 14, 7, 6, 4; LCD_CAM, DMA_CH2 |
| LED / BOOT | GPIO3 / GPIO0 |
| IR illumination | GPIO47, held off |

The attached camera was identified as **OV3660** (ID `0x3660`). DFRobot also
supplies a GC2145 variant; this firmware reports an error for unsupported
sensors. Camera setup is fixed at 1280x720 RGB565.

## Verification

The current firmware was flashed to the DFR1154 and captured a checksum-verified
1280×720 frame (1,843,200 bytes) over USB. A live stream on a joined network still requires verification.

Previously tested on the DFR1154 with the 160×120 USB capture firmware:

- Release build and USB flashing.
- OV3660 identification and complete 38,400-byte frame capture.
- Frame download, checksum validation, PNG conversion and visual inspection.
- Microphone capture of 16,000 bytes in approximately 511 ms.
- I2S speaker tone and recorded-audio DMA completion. Audible playback still
  requires confirmation with a connected speaker.

Development checks:

```sh
cargo fmt --check
cargo clippy --release -- -D warnings
```

The Xtensa linker currently emits a warning about an RWX LOAD segment; the
firmware builds and boots successfully.

## Generation and sources

The original template was generated from the parent directory with:

```sh
esp-generate --headless --skip-update-check --toolchain esp \
  -o esp32s3 -o log -o esp-backtrace dfr1154
```

`esp-hal`'s `unstable` feature is enabled for audio, camera and DMA APIs.
The sensor setup is a Rust adaptation of Espressif/OpenMV's OV3660 driver;
see [THIRD_PARTY.md](THIRD_PARTY.md) for sources and license notices.
