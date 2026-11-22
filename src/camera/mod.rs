//! OV3660 SCCB initialization and bounded RGB565 and JPEG frame capture.
// Sensor sequence adapted from esp32-camera / OpenMV's ov3660.c.
// Copyright (c) 2013/2014 Ibrahim Abdelkader <i.abdalkader@gmail.com>.
// Modified: fixed 720p RGB565 setup and bounded DMA capture in Rust.
// See ../../THIRD_PARTY.md and ../../licenses/ for attribution and licenses.
mod ov3660_registers;

use esp_hal::{
    Blocking,
    delay::Delay,
    dma::{
        BurstConfig, DmaBufError, DmaRxBuf, ExternalBurstConfig, InternalBurstConfig,
        aligned::DmaAlignedMut,
    },
    i2c::master::{Error as I2cError, I2c},
    lcd_cam::cam::{Camera, CameraTransfer},
    psram::Psram,
    time::{Duration, Instant},
};

pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 720;
pub const FRAME_BYTES: usize = WIDTH * HEIGHT * 2;
const RGB_TIMING: (u16, u16) = (3000, 1322);
const JPEG_TIMING: (u16, u16) = (2200, 1150);
const ADDRESS: u8 = 0x3c;

#[derive(Debug)]
pub enum Error {
    Bus(I2cError),
    UnsupportedSensor(u16),
    PsramUnavailable(usize),
    Buffer(DmaBufError),
    CaptureStart,
    CaptureTimeout(usize),
    IncompleteFrame(usize),
    IncompleteJpeg(usize),
}

impl From<I2cError> for Error {
    fn from(value: I2cError) -> Self {
        Self::Bus(value)
    }
}

pub struct BoardCamera<'d> {
    camera: Option<Camera<'d>>,
    buffer: Option<DmaRxBuf>,
    spare: Option<DmaRxBuf>,
    sccb: I2c<'d, Blocking>,
    jpeg_mode: Option<bool>,
    rgb_clocks: [u8; 2],
}

impl<'d> BoardCamera<'d> {
    pub fn new(
        camera: Camera<'d>,
        mut sccb: I2c<'d, Blocking>,
        psram: Psram,
    ) -> Result<Self, Error> {
        let delay = Delay::new();
        delay.delay_millis(100);
        let mut id = [0; 2];
        sccb.write_read(ADDRESS, &[0x30, 0x0a], &mut id)?;
        let id = u16::from_be_bytes(id);
        if id != 0x3660 {
            return Err(Error::UnsupportedSensor(id));
        }
        log::info!("OV3660 detected: 0x{:04x}", id);
        let (base, size) = psram.raw_parts();
        if size < 2 * FRAME_BYTES {
            return Err(Error::PsramUnavailable(size));
        }
        // SAFETY: This owns the PSRAM mapping for the firmware lifetime. The size
        // check covers both initialized, disjoint frame regions.
        let frames = unsafe {
            base.write_bytes(0, 2 * FRAME_BYTES);
            core::slice::from_raw_parts_mut(base, 2 * FRAME_BYTES)
        };
        let (frame, spare) = frames.split_at_mut(FRAME_BYTES);
        let (descriptors, spare_descriptors) =
            esp_hal::dma_descriptors_chunk_size!(FRAME_BYTES, FRAME_BYTES, 4032);
        let make_buffer = |descriptors, pixels| {
            DmaRxBuf::new_with_config(
                DmaAlignedMut::new(descriptors)?,
                DmaAlignedMut::new(pixels)?,
                BurstConfig {
                    external_memory: ExternalBurstConfig::Size64,
                    internal_memory: InternalBurstConfig::Enabled,
                },
            )
        };
        let buffer = make_buffer(descriptors, frame).map_err(Error::Buffer)?;
        let spare = make_buffer(spare_descriptors, spare).map_err(Error::Buffer)?;
        let mut board = Self {
            camera: Some(camera),
            buffer: Some(buffer),
            spare: Some(spare),
            sccb,
            jpeg_mode: Some(false),
            rgb_clocks: [0; 2],
        };
        board.write(0x3008, 0x82)?;
        delay.delay_millis(100);
        for &(register, value) in ov3660_registers::DEFAULT_REGISTERS {
            if register == 0xffff {
                delay.delay_millis(value as u32);
            } else {
                board.write(register, value)?;
            }
        }
        // RGB565, 16:9 sensor crop with ISP downscaling to 720p.
        board.write(0x501f, 0x01)?;
        board.write(0x4300, 0x61)?;
        board.write_pair(0x3800, 64, 242)?;
        board.write_pair(0x3804, 2015, 1333)?;
        board.write_pair(0x3808, WIDTH as u16, HEIGHT as u16)?;
        // The 720p RGB565 output needs additional horizontal blanking.
        board.write_pair(0x380c, RGB_TIMING.0, RGB_TIMING.1)?;
        board.write_pair(0x3810, 16, 6)?;
        let scale = board.read(0x5001)? | 0x20;
        board.write(0x5001, scale)?;
        for (reg, value) in [
            (0x3820, 0x46),
            (0x3821, 0x00),
            (0x4514, 0x88),
            (0x4520, 0xb0),
            (0x3814, 0x11),
            (0x3815, 0x11),
            // Large-frame PLL with a faster DVP clock for 1280 RGB565 pixels per line.
            (0x303a, 0x00),
            (0x303b, 0x04),
            (0x303c, 0x11),
            (0x303d, 0x02),
            (0x3824, 0x01),
            (0x460c, 0x22),
        ] {
            board.write(reg, value)?;
        }
        board.rgb_clocks = [board.read(0x3002)?, board.read(0x3006)?];
        delay.delay_millis(300);
        log::info!("Camera configured: {}x{} RGB565", WIDTH, HEIGHT);
        Ok(board)
    }

    fn write(&mut self, register: u16, value: u8) -> Result<(), I2cError> {
        let [hi, lo] = register.to_be_bytes();
        self.sccb.write(ADDRESS, &[hi, lo, value])
    }

    fn read(&mut self, register: u16) -> Result<u8, I2cError> {
        let mut byte = [0];
        self.sccb
            .write_read(ADDRESS, &register.to_be_bytes(), &mut byte)?;
        Ok(byte[0])
    }

    fn write_pair(&mut self, register: u16, x: u16, y: u16) -> Result<(), I2cError> {
        for (offset, byte) in x
            .to_be_bytes()
            .into_iter()
            .chain(y.to_be_bytes())
            .enumerate()
        {
            self.write(register + offset as u16, byte)?;
        }
        Ok(())
    }

    fn set_jpeg_mode(&mut self, jpeg: bool) -> Result<(), Error> {
        if self.configure_jpeg_mode(jpeg)? {
            Delay::new().delay_millis(350);
        }
        Ok(())
    }

    fn configure_jpeg_mode(&mut self, jpeg: bool) -> Result<bool, Error> {
        if self.jpeg_mode == Some(jpeg) {
            return Ok(false);
        }
        self.jpeg_mode = None;
        let registers = if jpeg {
            // YUV422 input to the OV3660 JPEG encoder, with a 10 MHz DVP clock.
            [
                (0x501f, 0x00),
                (0x4300, 0x30),
                (0x3002, 0x00),
                (0x3006, 0xff),
                (0x471c, 0x50),
                (0x3821, 0x20),
                (0x303b, 30),
                (0x303d, 0x30),
                (0x3824, 10),
            ]
        } else {
            [
                (0x501f, 0x01),
                (0x4300, 0x61),
                (0x3002, self.rgb_clocks[0]),
                (0x3006, self.rgb_clocks[1]),
                (0x471c, 0xd0),
                (0x3821, 0x00),
                (0x303b, 4),
                (0x303d, 0x02),
                (0x3824, 1),
            ]
        };
        for (address, value) in registers {
            self.write(address, value)?;
        }
        // JPEG's compressed DVP output needs less blanking than RGB565.
        let (line_length, frame_length) = if jpeg { JPEG_TIMING } else { RGB_TIMING };
        self.write_pair(0x380c, line_length, frame_length)?;
        if jpeg {
            self.write(0x4407, 16)?;
        }
        self.jpeg_mode = Some(jpeg);
        Ok(true)
    }

    pub fn capture(&mut self) -> Result<&[u8], Error> {
        self.set_jpeg_mode(false)?;
        let bytes = self.receive(false)?;
        if bytes != FRAME_BYTES {
            return Err(Error::IncompleteFrame(bytes));
        }
        Ok(self.buffer.as_ref().unwrap().as_slice())
    }

    pub fn capture_jpeg(&mut self) -> Result<&[u8], Error> {
        self.set_jpeg_mode(true)?;
        let started = Instant::now();
        let mut bytes = 0;
        for attempt in 1..=3 {
            bytes = self.receive(true)?;
            if let Some(len) = jpeg_length(self.buffer.as_ref().unwrap(), bytes) {
                if option_env!("JPEG_TIMING").is_some() {
                    log::info!(
                        "JPEG capture: {} ms, {} bytes, {} attempts",
                        started.elapsed().as_millis(),
                        len,
                        attempt
                    );
                }
                return Ok(&self.buffer.as_ref().unwrap().as_slice()[..len]);
            }
        }
        Err(Error::IncompleteJpeg(bytes))
    }

    pub async fn send_jpeg(
        &mut self,
        prefetched: Option<usize>,
        send: impl AsyncFnOnce(&[u8]) -> bool,
    ) -> Result<(bool, Option<usize>), Error> {
        if self.configure_jpeg_mode(true)? {
            embassy_time::Timer::after_millis(350).await;
        }
        let len = match prefetched {
            Some(len) => {
                core::mem::swap(&mut self.buffer, &mut self.spare);
                len
            }
            None => self.capture_jpeg_async().await?,
        };
        let next = Capture::start(&mut self.camera, &mut self.spare)?;
        let frame = &self.buffer.as_ref().unwrap().as_slice()[..len];
        if !send(frame).await {
            return Ok((false, None));
        }
        let started = Instant::now();
        while !next.done(true) && !next.expired() {
            embassy_time::Timer::after_millis(1).await;
        }
        let bytes = next.finish(true)?;
        // An empty or incomplete prefetch is retried by the normal capture path.
        let next_len = jpeg_length(self.spare.as_ref().unwrap(), bytes);
        if option_env!("JPEG_TIMING").is_some() {
            log::info!(
                "JPEG prefetch after send: {} ms wait, {} bytes",
                started.elapsed().as_millis(),
                next_len.unwrap_or(0)
            );
        }
        Ok((true, next_len))
    }

    // Waiting for a JPEG must leave the mic and Wi-Fi tasks runnable.
    async fn capture_jpeg_async(&mut self) -> Result<usize, Error> {
        let mut bytes = 0;
        for _ in 0..3 {
            let capture = Capture::start(&mut self.camera, &mut self.buffer)?;
            while !capture.done(true) && !capture.expired() {
                embassy_time::Timer::after_millis(1).await;
            }
            bytes = capture.finish(true)?;
            if let Some(len) = jpeg_length(self.buffer.as_ref().unwrap(), bytes) {
                return Ok(len);
            }
        }
        Err(Error::IncompleteJpeg(bytes))
    }

    fn receive(&mut self, jpeg: bool) -> Result<usize, Error> {
        let capture = Capture::start(&mut self.camera, &mut self.buffer)?;
        while !capture.done(jpeg) && !capture.expired() {}
        capture.finish(jpeg)
    }
}

fn jpeg_length(buffer: &DmaRxBuf, bytes: usize) -> Option<usize> {
    let data = buffer.as_slice().get(..bytes)?;
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    // DMA may have reached a second JPEG while the previous image was sent.
    data.windows(2)
        .position(|pair| pair == [0xff, 0xd9])
        .map(|end| end + 2)
}

struct Capture<'a, 'd> {
    camera: &'a mut Option<Camera<'d>>,
    buffer: &'a mut Option<DmaRxBuf>,
    transfer: Option<CameraTransfer<'d, DmaRxBuf>>,
    start: Instant,
}

impl<'a, 'd> Capture<'a, 'd> {
    fn start(
        camera: &'a mut Option<Camera<'d>>,
        buffer: &'a mut Option<DmaRxBuf>,
    ) -> Result<Self, Error> {
        match camera.take().unwrap().receive(buffer.take().unwrap()) {
            Ok(transfer) => Ok(Self {
                camera,
                buffer,
                transfer: Some(transfer),
                start: Instant::now(),
            }),
            Err((_, device, memory)) => {
                *camera = Some(device);
                *buffer = Some(memory);
                Err(Error::CaptureStart)
            }
        }
    }

    fn done(&self, jpeg: bool) -> bool {
        self.transfer.as_ref().unwrap().is_done()
            || (jpeg
                && esp_hal::peripherals::DMA::regs()
                    .ch(2)
                    .in_int()
                    .raw()
                    .read()
                    .in_suc_eof()
                    .bit_is_set())
    }

    fn expired(&self) -> bool {
        self.start.elapsed() >= Duration::from_secs(2)
    }

    fn finish(mut self, jpeg: bool) -> Result<usize, Error> {
        let timed_out = !self.done(jpeg);
        self.stop();
        let bytes = self.buffer.as_ref().unwrap().number_of_received_bytes();
        if timed_out {
            return Err(Error::CaptureTimeout(bytes));
        }
        Ok(bytes)
    }

    fn stop(&mut self) {
        if let Some(transfer) = self.transfer.take() {
            let (camera, buffer) = transfer.stop();
            *self.camera = Some(camera);
            *self.buffer = Some(buffer);
        }
    }
}

impl Drop for Capture<'_, '_> {
    fn drop(&mut self) {
        self.stop();
    }
}
