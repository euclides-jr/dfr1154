#!/usr/bin/env python3
"""Bridge the ESP32's live PDM microphone to a browser WebRTC audio track."""

import argparse
import asyncio
import fractions
import json
import logging
import os
import ssl
import sys
import time
from array import array
from pathlib import Path

from aiohttp import ClientError, ClientSession, ClientTimeout, MultipartReader, web
from aiortc import MediaStreamTrack, RTCConfiguration, RTCPeerConnection, RTCRtpSender, RTCSessionDescription
from aiortc.mediastreams import MediaStreamError
from aiortc.sdp import SessionDescription
from av import AudioFrame, CodecContext, Packet
from av.error import FFmpegError

SAMPLE_RATE = 16_000
SAMPLES = 320
MIC_PORT = 8096


def boost_pcm(pcm, gain):
    samples = array("h")
    samples.frombytes(pcm)
    if sys.byteorder != "little":
        samples.byteswap()
    knee = 24_576
    headroom = 32_767 - knee
    for index, sample in enumerate(samples):
        amplified = round(sample * gain)
        magnitude = abs(amplified)
        if magnitude > knee:
            excess = magnitude - knee
            magnitude = knee + excess * headroom // (excess + headroom)
            amplified = magnitude if amplified > 0 else -magnitude
        samples[index] = amplified
    # AudioFrame's s16 format takes native-endian samples.
    return samples.tobytes()


class Microphone(MediaStreamTrack):
    kind = "audio"

    def __init__(self, connect, gain, report):
        super().__init__()
        self.connect = connect
        self.gain = gain
        self.report = report
        self.reader = None
        self.writer = None
        self.started_at = None
        self.pts = 0

    async def recv(self):
        while self.readyState == "live":
            try:
                if self.reader is None:
                    reader, writer = await self.connect()
                    if self.readyState != "live":
                        writer.close()
                        raise MediaStreamError
                    self.reader, self.writer = reader, writer
                async with asyncio.timeout(2):
                    pcm = await self.reader.readexactly(2 * SAMPLES)
                self.report("audio", True)
                break
            except (OSError, EOFError, TimeoutError, web.HTTPBadGateway) as error:
                self.disconnect()
                self.report("audio", False)
                logging.info("Microphone read failed: %s", type(error).__name__)
                await asyncio.sleep(0.3)
        else:
            raise MediaStreamError

        now = time.monotonic()
        if self.started_at is None:
            self.started_at = now
        # Keep the live clock after a source dropout instead of replaying
        # delayed audio to fill the gap.
        expected = int((now - self.started_at) * SAMPLE_RATE)
        if expected - self.pts > SAMPLE_RATE // 10:
            self.pts = expected
        frame = AudioFrame(format="s16", layout="mono", samples=SAMPLES)
        frame.planes[0].update(boost_pcm(pcm, self.gain))
        frame.sample_rate = SAMPLE_RATE
        frame.time_base = fractions.Fraction(1, SAMPLE_RATE)
        frame.pts = self.pts
        self.pts += SAMPLES
        return frame

    def disconnect(self):
        if self.writer is not None:
            self.writer.close()
        self.reader = self.writer = None

    def stop(self):
        super().stop()
        self.disconnect()


class Camera(MediaStreamTrack):
    kind = "video"

    def __init__(self, board, fps, bitrate, report):
        super().__init__()
        self.url = f"http://{board}/stream"
        self.interval = 1 / fps
        self.bitrate = bitrate
        self.report = report
        self.decoder = CodecContext.create("mjpeg", "r")
        self.latest = asyncio.Queue(maxsize=1)
        self.reader = None
        self.started_at = None
        self.not_before = 0.0
        self.hardware = False
        self.encoder = None
        self.pending = []
        self.worker = None

    async def capture(self):
        timeout = ClientTimeout(total=None, connect=4, sock_read=5)
        async with ClientSession(timeout=timeout) as client:
            while self.readyState == "live":
                try:
                    async with client.get(self.url) as response:
                        response.raise_for_status()
                        parts = MultipartReader.from_response(response)
                        async for part in parts:
                            length = int(part.headers.get("Content-Length", "0"))
                            if not 0 < length <= 2_000_000:
                                raise ValueError("invalid JPEG part size")
                            jpeg = await part.read()
                            if len(jpeg) != length:
                                raise ValueError("incomplete JPEG frame")
                            if self.latest.full():
                                self.latest.get_nowait()
                            self.latest.put_nowait((time.monotonic(), bytes(jpeg)))
                except (ClientError, OSError, TimeoutError, ValueError) as error:
                    logging.warning("Camera stream interrupted: %s", type(error).__name__)
                self.report("video", False)
                await asyncio.sleep(0.5)

    def process(self, jpeg, timestamp):
        frame = self.decoder.decode(Packet(jpeg))[0]
        frame.pts = timestamp
        frame.time_base = fractions.Fraction(1, 90_000)
        if self.hardware:
            try:
                if self.encoder is None:
                    encoder = CodecContext.create("h264_v4l2m2m", "w")
                    encoder.width = frame.width
                    encoder.height = frame.height
                    encoder.pix_fmt = "nv12"
                    encoder.time_base = frame.time_base
                    encoder.framerate = fractions.Fraction(round(1 / self.interval))
                    encoder.bit_rate = self.bitrate
                    encoder.gop_size = round(1 / self.interval)
                    encoder.max_b_frames = 0
                    encoder.profile = "Baseline"
                    encoder.open()
                    self.encoder = encoder
                    logging.info("Video encoding with h264_v4l2m2m at %dx%d, target %d bit/s",
                                 frame.width, frame.height, self.bitrate)
                return [packet for packet in self.encoder.encode(frame.reformat(format="nv12"))
                        if packet.size and packet.pts is not None]
            except FFmpegError as error:
                logging.warning("Hardware H.264 unavailable, using software: %s", error)
                self.encoder = None
                self.hardware = False
        return [frame.reformat(format="yuv420p")]

    async def recv(self):
        if self.readyState != "live":
            raise MediaStreamError
        if self.reader is None:
            self.reader = asyncio.create_task(self.capture())
            self.started_at = time.monotonic()
        while self.readyState == "live":
            if self.pending:
                return self.pending.pop(0)
            await asyncio.sleep(max(0, self.not_before - time.monotonic()))
            captured_at, jpeg = await self.latest.get()
            self.not_before = time.monotonic() + self.interval
            timestamp = round((captured_at - self.started_at) * 90_000)
            self.worker = asyncio.create_task(asyncio.to_thread(self.process, jpeg, timestamp))
            try:
                self.pending = await asyncio.shield(self.worker)
                if self.pending:
                    self.report("video", True)
            except FFmpegError as error:
                self.report("video", False)
                logging.warning("Invalid camera frame: %s", error)
            finally:
                if self.worker.done():
                    self.worker = None
        raise MediaStreamError

    def stop(self):
        super().stop()
        if self.reader is not None:
            self.reader.cancel()
            self.reader = None


    async def release_encoder(self):
        if self.worker is not None:
            await asyncio.gather(self.worker, return_exceptions=True)
        self.encoder = None


def lan_offer(sdp):
    """Let ICE learn peer-reflexive addresses when browser mDNS is unreachable."""
    lines = sdp.splitlines()
    hidden = set()
    for line in lines:
        if line.startswith("a=candidate:"):
            fields = line.split()
            if len(fields) >= 8 and fields[4].lower().rstrip(".").endswith(".local"):
                hidden.add(line)
    if not hidden:
        return sdp
    # With end-of-candidates set, aioice fails immediately when all offered
    # names are unreachable. Keep its checklist open for the browser's
    # authenticated STUN checks against the advertised NanoPi addresses.
    return "\r\n".join(
        line for line in lines if line not in hidden and line != "a=end-of-candidates"
    ) + "\r\n"


def application(board, token, public_origin=None, gain=4.0, fps=8, hardware_video=False, bitrate=2_500_000):
    peer = None
    tracks = []
    lock = asyncio.Lock()

    @web.middleware
    async def restrict_origin(request, handler):
        origin = request.headers.get("Origin")
        if request.method == "POST" and origin is not None and origin != (public_origin or f"{request.scheme}://{request.host}"):
            raise web.HTTPForbidden()
        response = await handler(request)
        response.headers["Cache-Control"] = "no-store"
        return response

    async def close_peer():
        nonlocal peer, tracks
        current, peer = peer, None
        previous, tracks = tracks, []
        for track in previous:
            track.stop()
        if current is not None:
            await current.close()
        for track in previous:
            if isinstance(track, Camera):
                await track.release_encoder()

    async def index(_request):
        return web.Response(
            text=Path(__file__).with_name("index.html").read_text(encoding="utf-8"),
            content_type="text/html",
        )

    async def open_microphone():
        # The board serves one listener and may still be closing the old TCP
        # connection when a browser switches transports.
        for attempt in range(3):
            writer = None
            try:
                async with asyncio.timeout(1.5):
                    reader, writer = await asyncio.open_connection(board, MIC_PORT)
                    writer.write(token + b"\n")
                    await writer.drain()
                    if await reader.readexactly(5) != b"MIC1\n":
                        raise OSError("invalid microphone response")
                return reader, writer
            except asyncio.CancelledError:
                if writer is not None:
                    writer.close()
                raise
            except (OSError, EOFError, TimeoutError):
                if writer is not None:
                    writer.close()
                if attempt < 2:
                    await asyncio.sleep(0.2)
        raise web.HTTPBadGateway(text="Board microphone unavailable")

    async def offer(request):
        nonlocal peer, tracks
        try:
            data = await request.json()
            if data["type"] != "offer" or not isinstance(data["sdp"], str):
                raise ValueError
        except (ValueError, KeyError, TypeError):
            raise web.HTTPBadRequest(text="Expected a WebRTC offer") from None

        async with lock:
            await close_peer()
            peer = current = RTCPeerConnection(RTCConfiguration(iceServers=[]))
            media_state = {"audio": False, "video": False}
            status_channel = None

            def send_status():
                if status_channel is not None and status_channel.readyState == "open":
                    status_channel.send(json.dumps(media_state))

            def report(kind, online):
                if media_state[kind] != online:
                    media_state[kind] = online
                    send_status()

            @current.on("datachannel")
            def data_channel(channel):
                nonlocal status_channel
                if channel.label == "monitor-status":
                    status_channel = channel
                    channel.on("open", send_status)
                    send_status()

            microphone = Microphone(open_microphone, gain, report)
            tracks = current_tracks = [microphone]

            @current.on("connectionstatechange")
            async def state_changed():
                logging.info("WebRTC connection state: %s", current.connectionState)
                if current.connectionState in {"failed", "closed", "disconnected"}:
                    for track in current_tracks:
                        track.stop()
                    if current.connectionState != "closed":
                        await current.close()
                    for track in current_tracks:
                        if isinstance(track, Camera):
                            await track.release_encoder()

            try:
                async with asyncio.timeout(10):
                    offer_sdp = lan_offer(data["sdp"])
                    offered_video = any(media.kind == "video" and media.port
                                        for media in SessionDescription.parse(offer_sdp).media)
                    camera = None
                    if offered_video:
                        camera = Camera(board, fps, bitrate, report)
                        tracks.append(camera)
                        video = current.addTransceiver(camera, direction="sendonly")
                        codecs = RTCRtpSender.getCapabilities("video").codecs
                        preferred = "video/h264" if hardware_video else "video/vp8"
                        video.setCodecPreferences(
                            sorted(codecs, key=lambda codec: codec.mimeType.lower() != preferred)
                        )
                    await current.setRemoteDescription(RTCSessionDescription(sdp=offer_sdp, type=data["type"]))
                    if any(transceiver.kind == "audio" for transceiver in current.getTransceivers()):
                        current.addTrack(microphone)
                    answer = await current.createAnswer()
                    if camera is not None and hardware_video:
                        video_answer = next(media for media in SessionDescription.parse(answer.sdp).media
                                            if media.kind == "video")
                        camera.hardware = video_answer.rtp.codecs[0].mimeType.lower() == "video/h264"
                    await current.setLocalDescription(answer)

            except (Exception, asyncio.CancelledError):
                await close_peer()
                raise

            async def expire_unconnected():
                await asyncio.sleep(18)
                if current.connectionState in {"new", "connecting"}:
                    logging.info("WebRTC connection setup expired")
                    await current.close()

            watchdog = asyncio.create_task(expire_unconnected())

            @current.on("connectionstatechange")
            def cancel_watchdog():
                if current.connectionState in {"connected", "closed", "failed"}:
                    if asyncio.current_task() is not watchdog:
                        watchdog.cancel()

            return web.json_response({
                "sdp": current.localDescription.sdp,
                "type": current.localDescription.type,
            })

    async def stop(_request):
        async with lock:
            await close_peer()
        return web.Response(status=204)

    async def cleanup(_app):
        await close_peer()

    app = web.Application(middlewares=[restrict_origin], client_max_size=64 * 1024)
    app.add_routes([web.get("/", index), web.post("/offer", offer), web.post("/stop", stop)])
    app.on_cleanup.append(cleanup)
    return app


def main():
    logging.basicConfig(level=logging.INFO)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--board", default="dfrobot.local")
    parser.add_argument("--hardware-video", action="store_true", help="Use a V4L2 H.264 encoder if available")
    parser.add_argument("--video-bitrate", type=int, default=2_500_000, help="Hardware H.264 bitrate in bits per second")
    parser.add_argument("--video-fps", type=int, default=8, choices=range(1, 16))
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8443)
    parser.add_argument("--cert", help="PEM certificate trusted by the phone")
    parser.add_argument("--key", help="PEM certificate private key")
    parser.add_argument("--public-origin", help="HTTPS origin when TLS is terminated by a reverse proxy")
    args = parser.parse_args()
    if bool(args.cert) != bool(args.key) or (not args.cert and not args.public_origin):
        parser.error("provide --cert and --key, or --public-origin behind an HTTPS proxy")
    token = os.environ.get("MIC_STREAM_TOKEN", "")
    if not 32 <= len(token) <= 128 or not all(33 <= ord(c) <= 126 for c in token):
        parser.error("MIC_STREAM_TOKEN must contain 32–128 printable ASCII characters")
    try:
        gain = float(os.environ.get("MIC_GAIN", "4"))
        if not 1 <= gain <= 8:
            raise ValueError
    except ValueError:
        parser.error("MIC_GAIN must be a multiplier between 1 and 8")
    logging.info("Microphone gain: %.2fx", gain)
    context = None
    if args.cert:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(args.cert, args.key)
    web.run_app(application(args.board, token.encode("ascii"), args.public_origin, gain, args.video_fps, args.hardware_video, args.video_bitrate),
                host=args.host, port=args.port, ssl_context=context)


if __name__ == "__main__":
    main()
