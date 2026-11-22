use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read as _, Write as _};

pub const CHUNK_BYTES: usize = 640;
const PORT: u16 = 8096;
static LISTENING: AtomicBool = AtomicBool::new(false);
static PCM: Channel<CriticalSectionRawMutex, [u8; CHUNK_BYTES], 4> = Channel::new();

pub fn start(spawner: Spawner, stack: Stack<'static>) {
    if let Some(secret) = option_env!("MIC_STREAM_TOKEN") {
        if (32..=128).contains(&secret.len()) && secret.bytes().all(|byte| byte.is_ascii_graphic())
        {
            spawner.spawn(serve(stack, secret.as_bytes()).unwrap());
        } else {
            log::warn!("MIC_STREAM_TOKEN must contain 32–128 printable ASCII characters");
        }
    }
}

pub fn listening() -> bool {
    LISTENING.load(Ordering::Relaxed)
}

pub fn publish(chunk: [u8; CHUNK_BYTES]) {
    let _ = PCM.try_send(chunk);
}

#[embassy_executor::task]
async fn serve(stack: Stack<'static>, token: &'static [u8]) {
    let mut rx = [0; 256];
    let mut tx = [0; 1280];
    log::info!("Microphone gateway endpoint enabled on TCP port {}", PORT);
    loop {
        stack.wait_config_up().await;
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_nagle_enabled(false);
        if socket.accept(PORT).await.is_err() {
            embassy_time::Timer::after_millis(100).await;
            continue;
        }
        let mut request = [0; 129];
        let request = &mut request[..token.len() + 1];
        if with_timeout(Duration::from_secs(2), socket.read_exact(request)).await != Ok(Ok(())) {
            continue;
        }
        let mismatch = token
            .iter()
            .zip(request.iter())
            .fold(0, |difference, (expected, actual)| {
                difference | (expected ^ actual)
            });
        if mismatch != 0 || request[token.len()] != b'\n' {
            continue;
        }
        if with_timeout(Duration::from_secs(1), socket.write_all(b"MIC1\n")).await != Ok(Ok(())) {
            continue;
        }
        PCM.clear();
        LISTENING.store(true, Ordering::Relaxed);
        let mut unexpected = [0];
        loop {
            // Reading also detects a remote close during a capture failure.
            use embassy_futures::select::{Either, select};
            let Either::First(Ok(chunk)) = select(
                with_timeout(Duration::from_secs(1), PCM.receive()),
                socket.read(&mut unexpected),
            )
            .await
            else {
                break;
            };
            if with_timeout(Duration::from_secs(1), socket.write_all(&chunk)).await != Ok(Ok(())) {
                break;
            }
        }
        LISTENING.store(false, Ordering::Relaxed);
    }
}
