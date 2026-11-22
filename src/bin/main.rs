#![no_std]
#![no_main]

#[path = "wifi/mod.rs"]
mod wifi;

use embassy_executor::Spawner;
use embassy_futures::select::select;
use embassy_net::tcp::TcpSocket;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    i2s::master::{Channels, DataFormat, I2s, PdmConfig, PdmRxConfig, PdmSlotMode, TdmConfig},
    lcd_cam::{
        LcdCam,
        cam::{Camera, Config as CameraConfig},
    },
    time::Rate,
    timer::timg::TimerGroup,
    usb::usb_serial_jtag::UsbSerialJtag,
};
use log::info;
use wifi::{mic, sendspin::pcm};

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let psram = esp_hal::psram::Psram::new(p.PSRAM, Default::default());
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    let timers = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timers.timer0, p.FROM_CPU_INTR0);

    let led = Output::new(p.GPIO3, Level::Low, OutputConfig::default());
    let boot = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let mut usb = UsbSerialJtag::new(p.USB_DEVICE);
    let _ir = Output::new(p.GPIO47, Level::Low, OutputConfig::default());

    let camera = Camera::new(
        LcdCam::new(p.LCD_CAM).cam,
        p.DMA_CH2,
        CameraConfig::default().with_frequency(Rate::from_mhz(20)),
    )
    .unwrap()
    .with_master_clock(p.GPIO5)
    .with_pixel_clock(p.GPIO15)
    .with_vsync(p.GPIO1)
    .with_h_enable(p.GPIO2)
    .with_data0(p.GPIO16)
    .with_data1(p.GPIO18)
    .with_data2(p.GPIO21)
    .with_data3(p.GPIO17)
    .with_data4(p.GPIO14)
    .with_data5(p.GPIO7)
    .with_data6(p.GPIO6)
    .with_data7(p.GPIO4);
    let sccb = I2c::new(
        p.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(100)),
    )
    .unwrap()
    .with_sda(p.GPIO8)
    .with_scl(p.GPIO9);
    if option_env!("SENDSPIN_SERVER_IP").is_some() {
        let (base, size) = psram.raw_parts();
        let audio_offset = 2 * dfr1154::camera::FRAME_BYTES;
        if size >= audio_offset + pcm::PSRAM_BYTES {
            // The camera owns the first two frame regions. Audio uses the following disjoint region.
            let bytes = unsafe {
                let start = base.add(audio_offset);
                start.write_bytes(0, pcm::PSRAM_BYTES);
                core::slice::from_raw_parts_mut(start, pcm::PSRAM_BYTES)
            };
            pcm::init(bytes);
        }
    }
    let mut camera = match dfr1154::camera::BoardCamera::new(camera, sccb, psram) {
        Ok(camera) => Some(camera),
        Err(error) => {
            log::error!("Camera initialization: {:?}", error);
            None
        }
    };

    let mic = I2s::new_pdm(
        p.I2S0,
        p.DMA_CH0,
        PdmConfig::rx_only(PdmRxConfig::new_pcm_default(
            Rate::from_hz(dfr1154::audio::MIC_SAMPLE_RATE),
            PdmSlotMode::Mono,
        )),
    )
    .unwrap()
    .i2s_rx
    .with_clk(p.GPIO38)
    .with_din(p.GPIO39)
    .build();
    let speaker = I2s::new(
        p.I2S1,
        p.DMA_CH1,
        TdmConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(dfr1154::audio::SAMPLE_RATE))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO),
    )
    .unwrap()
    .i2s_tx
    .with_bclk(p.GPIO45)
    .with_ws(p.GPIO46)
    .with_dout(p.GPIO42)
    .build();
    info!("Audio interfaces initialized");
    let mut audio = dfr1154::audio::Audio::new(mic, speaker);
    match audio.play_stream_test_tone().await {
        Ok(()) => info!("Streaming DMA tone complete"),
        Err(error) => log::error!("Audio test: {:?}", error),
    }
    spawner.spawn(audio_output(audio, led).unwrap());
    let stack = wifi::start(spawner, p.WIFI);
    if let Some(camera) = camera.as_mut() {
        report_frame(camera);
    }
    info!("Ready: BOOT or 'a'=record/play, 't'=tone, 'f'=download frame, 'c'=capture stats");
    let mut was_pressed = false;
    let mut poll_controls = |camera: Option<&mut dfr1154::camera::BoardCamera<'_>>| {
        let pressed = boot.is_low();
        let command = usb.read_byte().ok();
        if (pressed && !was_pressed) || command == Some(b'a') {
            AUDIO_COMMAND.signal(b'a');
        } else if command == Some(b't') {
            AUDIO_COMMAND.signal(b't');
        }
        if let Some(camera) = camera {
            if command == Some(b'c') {
                report_frame(camera);
            }
            if command == Some(b'f') {
                match camera.capture() {
                    Ok(frame) => dump_frame(frame),
                    Err(error) => log::error!("Camera: {:?}", error),
                }
            }
        } else if matches!(command, Some(b'c' | b'f')) {
            log::error!("Camera is unavailable; check the initialization log");
        }
        was_pressed = pressed;
    };
    let mut rx = [0; 1024];
    let mut tx = [0; 4096];
    loop {
        let mut socket = stack.map(|stack| TcpSocket::new(stack, &mut rx, &mut tx));
        let accept = async {
            match socket.as_mut() {
                Some(socket) => socket.accept(80).await,
                None => core::future::pending().await,
            }
        };
        let usb_commands = async {
            loop {
                poll_controls(camera.as_mut());
                Timer::after_millis(10).await;
            }
        };
        if matches!(
            select(accept, usb_commands).await,
            embassy_futures::select::Either::First(Ok(()))
        ) && let Some(socket) = socket.as_mut()
        {
            wifi::serve(socket, camera.as_mut(), |camera| {
                poll_controls(Some(camera))
            })
            .await;
        }
    }
}

static AUDIO_COMMAND: Signal<CriticalSectionRawMutex, u8> = Signal::new();

#[embassy_executor::task]
async fn audio_output(mut audio: dfr1154::audio::Audio<'static>, mut led: Output<'static>) {
    use dfr1154::audio::STREAM_BUFFER_BYTES;
    let mut playing = None;
    let mut played_bytes = 0;
    let mut drained = None;
    let mut mic_chunk = [0; mic::CHUNK_BYTES];
    let mut mic_used = 0;
    loop {
        let listening = mic::listening();
        if listening {
            // A DMA descriptor can contain more than one network chunk.
            while let Ok(count) = audio.read_live_mic(&mut mic_chunk[mic_used..]) {
                mic_used += count;
                if mic_used != mic_chunk.len() {
                    break;
                }
                mic::publish(mic_chunk);
                mic_used = 0;
                embassy_futures::yield_now().await;
            }
        } else {
            audio.stop_live_mic();
            mic_used = 0;
        }
        if let Some(status) = pcm::status() {
            if let Some(generation) = drained {
                if generation != status.generation {
                    drained = None;
                } else if status.ended && status.len == 0 {
                    log::info!("Sendspin PCM playback completed ({played_bytes} bytes)");
                    drained = None;
                } else if status.len != 0 {
                    log::warn!("Sendspin speaker underrun after {played_bytes} bytes");
                    drained = None;
                }
            }
            if playing.is_some_and(|generation| generation != status.generation) {
                audio.stop_network_playback();
                playing = None;
            }
            if playing.is_some() {
                if !audio.refill_network_playback(status.len, status.ended, |target| {
                    let count = pcm::pop(target);
                    played_bytes += count;
                    count
                }) {
                    if status.ended && status.len == 0 {
                        log::info!("Sendspin PCM playback completed ({played_bytes} bytes)");
                    } else if status.len == 0 {
                        drained = playing;
                    } else {
                        log::warn!(
                            "Sendspin speaker underrun after {played_bytes} bytes ({} bytes queued)",
                            status.len
                        );
                    }
                    playing = None;
                }
            } else {
                let now = Instant::now().as_micros() as i64;
                if status.len != 0 && status.first_time <= now {
                    pcm::discard_before(now);
                    if let Some(ready) = pcm::status()
                        && (ready.len >= STREAM_BUFFER_BYTES || (ready.ended && ready.len != 0))
                    {
                        played_bytes = 0;
                        match audio.start_network_playback(|target| {
                            played_bytes = pcm::pop(target);
                            played_bytes
                        }) {
                            Ok(()) => {
                                playing = Some(ready.generation);
                                log::info!("Sendspin PCM playback started");
                            }
                            Err(error) => log::warn!("Sendspin speaker start failed: {:?}", error),
                        }
                    }
                }
            }
        }
        if let Some(command) = AUDIO_COMMAND.try_take() {
            if playing.is_some()
                || pcm::status().is_some_and(|status| status.len != 0)
                || (listening && command == b'a')
            {
                log::info!("USB audio test skipped while network audio is in use");
            } else {
                let result = if command == b'a' {
                    led.set_high();
                    let result = audio.record_and_play();
                    led.set_low();
                    result
                } else {
                    audio.play_tone()
                };
                if let Err(error) = result {
                    log::warn!("USB audio test failed: {:?}", error);
                }
            }
        }
        if playing.is_some() || pcm::status().is_some_and(|status| status.len != 0) {
            Timer::after_micros(500).await;
        } else {
            Timer::after_millis(5).await;
        }
    }
}

fn report_frame(camera: &mut dfr1154::camera::BoardCamera<'_>) {
    match camera.capture() {
        Ok(frame) => info!(
            "Camera captured {} bytes; checksum={:08x}",
            frame.len(),
            checksum(frame)
        ),
        Err(error) => log::error!("Camera: {:?}", error),
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c9dc5u32, |hash, byte| {
        (hash ^ *byte as u32).wrapping_mul(0x01000193)
    })
}

fn dump_frame(frame: &[u8]) {
    use dfr1154::camera::{HEIGHT, WIDTH};
    esp_println::println!("FRAME {} {} RGB565_BE {}", WIDTH, HEIGHT, frame.len());
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for chunk in frame.chunks(64) {
        let mut line = [b'0'; 128];
        for (byte, pair) in chunk.iter().zip(line.chunks_exact_mut(2)) {
            pair[0] = HEX[(byte >> 4) as usize];
            pair[1] = HEX[(byte & 15) as usize];
        }
        esp_println::println!(
            "DATA {}",
            core::str::from_utf8(&line[..chunk.len() * 2]).unwrap()
        );
    }
    esp_println::println!("END {:08x}", checksum(frame));
}
