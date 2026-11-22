# Third-party sources

The OV3660 defaults in `src/camera/ov3660_registers.rs` and the sensor setup
sequence in `src/camera/mod.rs` are adapted from
[Espressif's esp32-camera](https://github.com/espressif/esp32-camera), commit
`b3f7fe802f41f17fd7a104d517c53f1f62c8414d`:

- `sensors/private_include/ov3660_settings.h`: the default register table was
  converted from C to Rust. The repository's Apache-2.0 license is included in
  `licenses/esp32-camera-APACHE-2.0.txt`.
- `sensors/ov3660.c`: the reset, RGB565, window/binning/scaling and PLL setup
  were adapted to a fixed 160x120 mode using Rust SCCB transactions. This file
  carries the OpenMV MIT notice:

  Copyright (c) 2013/2014 Ibrahim Abdelkader <i.abdalkader@gmail.com>.

  The MIT license is included in `licenses/OpenMV-MIT.txt`.

Board pin assignments follow the
[DFRobot DFR1154 camera example](https://github.com/DFRobot/DFR1154_Examples/blob/main/5.3%20CameraWebServer/source%20code/5_3/5_3.ino)
and [audio tutorial](https://wiki.dfrobot.com/dfr1154/docs/21377).
