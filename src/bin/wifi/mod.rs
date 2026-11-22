mod mdns;
pub(crate) mod mic;
pub(crate) mod sendspin;

use core::fmt::Write;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources, tcp::TcpSocket};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::Write as _;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, Interface, WifiController, sta::StationConfig,
};
use static_cell::StaticCell;

pub fn start(
    spawner: Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
) -> Option<Stack<'static>> {
    let (Some(ssid), Some(password)) = (option_env!("WIFI_SSID"), option_env!("WIFI_PASSWORD"))
    else {
        log::info!("Set WIFI_SSID and WIFI_PASSWORD at build time to enable streaming");
        return None;
    };
    let (Ok(ssid), Ok(password)) = (ssid.try_into(), password.try_into()) else {
        log::error!("Invalid Wi-Fi credential length");
        return None;
    };
    let mut controller = WifiController::new(wifi, Default::default()).unwrap();
    let config = StationConfig::default()
        .with_ssid(ssid)
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(password));
    if let Err(error) = controller.set_config(&Config::Station(config)) {
        log::error!("Wi-Fi configuration failed: {:?}", error);
        return None;
    }

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        u64::from(esp_hal::rng::Rng::new().random()) << 32
            | u64::from(esp_hal::rng::Rng::new().random()),
    );
    spawner.spawn(network(runner).unwrap());
    spawner.spawn(connect(controller, stack).unwrap());
    spawner.spawn(mdns::respond(stack).unwrap());
    sendspin::start(spawner, stack);
    mic::start(spawner, stack);
    Some(stack)
}

#[embassy_executor::task]
async fn network(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn connect(mut controller: WifiController<'static>, stack: Stack<'static>) {
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                use embassy_futures::select::{Either, select};
                let disconnected = controller.wait_for_disconnect_async();
                let show_address = async {
                    stack.wait_config_up().await;
                    if let Some(config) = stack.config_v4() {
                        log::info!("Camera viewer: http://{}/", config.address.address());
                    }
                };
                if let Either::Second(()) = select(disconnected, show_address).await {
                    let _ = controller.wait_for_disconnect_async().await;
                }
            }
            Err(error) => log::warn!("Wi-Fi connection failed: {:?}", error),
        }
        Timer::after_secs(2).await;
    }
}

pub async fn serve(
    socket: &mut TcpSocket<'_>,
    camera: Option<&mut dfr1154::camera::BoardCamera<'_>>,
    poll_controls: impl FnMut(&mut dfr1154::camera::BoardCamera<'_>),
) {
    let request = with_timeout(Duration::from_secs(5), read_request(socket)).await;
    let complete = match (request, camera) {
        (Ok(Ok(Some(Request::Stream))), Some(camera)) => {
            stream(socket, camera, poll_controls).await;
            false
        }
        (Ok(Ok(Some(request))), camera) => {
            with_timeout(Duration::from_secs(5), respond(socket, camera, request)).await
                == Ok(Ok(()))
        }
        _ => false,
    };
    if complete {
        socket.close();
        let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
    } else {
        socket.abort();
        let _ = with_timeout(Duration::from_millis(100), socket.flush()).await;
    }
}

enum Request {
    Viewer,
    Snapshot,
    Stream,
    Other,
}

async fn read_request(
    socket: &mut TcpSocket<'_>,
) -> Result<Option<Request>, embassy_net::tcp::Error> {
    let mut request = [0; 1024];
    let mut len = 0;
    while !request[..len].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        if len == request.len() {
            return Ok(None);
        }
        let count = socket.read(&mut request[len..]).await?;
        if count == 0 {
            return Ok(None);
        }
        len += count;
    }

    Ok(Some(match &request[..len] {
        data if data.starts_with(b"GET / ") => Request::Viewer,
        data if data.starts_with(b"GET /frame ") => Request::Snapshot,
        data if data.starts_with(b"GET /stream ") => Request::Stream,
        _ => Request::Other,
    }))
}

async fn stream<'d>(
    socket: &mut TcpSocket<'_>,
    camera: &mut dfr1154::camera::BoardCamera<'d>,
    mut poll_controls: impl FnMut(&mut dfr1154::camera::BoardCamera<'d>),
) {
    const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    let timeout = Duration::from_secs(5);
    if with_timeout(timeout, socket.write_all(RESPONSE)).await != Ok(Ok(())) {
        return;
    }
    let mut prefetched = None;
    while socket.may_send() {
        poll_controls(camera);
        match camera
            .send_jpeg(prefetched, async |frame| {
                let mut headers = Header {
                    bytes: [0; 256],
                    len: 0,
                };
                write!(
                    headers,
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    frame.len()
                )
                .unwrap();
                let started = embassy_time::Instant::now();
                let send = async {
                    socket.write_all(&headers.bytes[..headers.len]).await?;
                    socket.write_all(frame).await?;
                    socket.write_all(b"\r\n").await
                };
                let sent = with_timeout(timeout, send).await == Ok(Ok(()));
                if option_env!("JPEG_TIMING").is_some() {
                    log::info!(
                        "JPEG TCP enqueue: {} ms, {} bytes, success={}",
                        started.elapsed().as_millis(),
                        frame.len(),
                        sent
                    );
                }
                sent
            })
            .await
        {
            Ok((true, next)) => prefetched = next,
            Ok((false, _)) => return,
            Err(error) => {
                log::warn!("JPEG capture: {:?}", error);
                return;
            }
        }
    }
}

async fn respond(
    socket: &mut TcpSocket<'_>,
    camera: Option<&mut dfr1154::camera::BoardCamera<'_>>,
    request: Request,
) -> Result<(), embassy_net::tcp::Error> {
    let (status, content_type, body): (&str, &str, &[u8]) = if matches!(request, Request::Viewer) {
        (
            "200 OK",
            "text/html; charset=utf-8",
            include_bytes!("viewer.html"),
        )
    } else if matches!(request, Request::Snapshot) {
        match camera.and_then(|camera| {
            camera
                .capture_jpeg()
                .inspect_err(|error| log::warn!("JPEG capture: {:?}", error))
                .ok()
        }) {
            Some(frame) => ("200 OK", "image/jpeg", frame),
            None => (
                "503 Service Unavailable",
                "text/plain",
                b"Camera unavailable",
            ),
        }
    } else if matches!(request, Request::Stream) {
        (
            "503 Service Unavailable",
            "text/plain",
            b"Camera unavailable",
        )
    } else {
        ("404 Not Found", "text/plain", b"Not found")
    };

    use dfr1154::camera::{HEIGHT, WIDTH};
    let mut headers = Header {
        bytes: [0; 256],
        len: 0,
    };
    write!(
        headers,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nX-Width: {WIDTH}\r\nX-Height: {HEIGHT}\r\n\r\n",
        body.len()
    ).unwrap();
    socket.write_all(&headers.bytes[..headers.len]).await?;
    socket.write_all(body).await
}

struct Header {
    bytes: [u8; 256],
    len: usize,
}

impl Write for Header {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
