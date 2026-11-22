//! Board audio: 16 kHz PDM microphone and 48 kHz stereo I2S playback.
use embassy_time::Timer;
use esp_hal::{
    Blocking,
    dma::{DmaRxStreamBuf, DmaTxBuf, DmaTxStreamBuf},
    i2s::master::{I2sRx, I2sRxDmaTransfer, I2sTx, I2sTxDmaTransfer, TdmUnitConfig},
    time::{Duration, Instant, Rate},
};

pub const MIC_SAMPLE_RATE: u32 = 16_000;
pub const SAMPLE_RATE: u32 = 48_000;
pub const RECORD_BYTES: usize = 16_000;
const STEREO_FRAME_BYTES: usize = 4;
const STREAM_CHUNK_BYTES: usize = 2048;
pub const STREAM_BUFFER_BYTES: usize = STREAM_CHUNK_BYTES * 8;

#[derive(Debug)]
pub enum Error {
    CaptureStart,
    CaptureTimeout(usize),
    PlaybackStart,
    PlaybackConfig,
    PlaybackTimeout,
    PlaybackDma,
}

pub struct Audio<'d> {
    mic: Option<I2sRx<'d, Blocking>>,
    speaker: Option<I2sTx<'d, Blocking>>,
    rx: Option<DmaRxStreamBuf>,
    tx: Option<DmaTxBuf>,
    stream_tx: Option<DmaTxStreamBuf>,
    mic_stream: Option<I2sRxDmaTransfer<'d, Blocking, DmaRxStreamBuf>>,
    network_tx: Option<I2sTxDmaTransfer<'d, Blocking, DmaTxStreamBuf>>,
}

impl<'d> Audio<'d> {
    pub fn new(mic: I2sRx<'d, Blocking>, speaker: I2sTx<'d, Blocking>) -> Self {
        Self {
            mic: Some(mic),
            speaker: Some(speaker),
            // 1024-byte descriptors preserve PCM word alignment; consume and recycle
            // them while recording instead of relying on the hardware EOF counter.
            rx: Some(esp_hal::dma_rx_stream_buffer!(4096, 1024)),
            tx: Some(esp_hal::dma_tx_buffer!(RECORD_BYTES * 2).unwrap()),
            network_tx: None,
            mic_stream: None,
            stream_tx: Some(esp_hal::dma_tx_stream_buffer!(
                STREAM_BUFFER_BYTES,
                STREAM_CHUNK_BYTES
            )),
        }
    }

    pub fn read_live_mic(&mut self, target: &mut [u8]) -> Result<usize, Error> {
        if self
            .mic_stream
            .as_ref()
            .is_some_and(|capture| capture.is_done())
        {
            self.stop_live_mic();
        }
        if self.mic_stream.is_none() {
            let mic = self.mic.take().unwrap();
            let rx = self.rx.take().unwrap();
            match mic.read(rx) {
                Ok(capture) => self.mic_stream = Some(capture),
                Err((_, mic, rx)) => {
                    self.mic = Some(mic);
                    self.rx = Some(rx);
                    return Err(Error::CaptureStart);
                }
            }
        }
        Ok(self.mic_stream.as_mut().unwrap().pop(target))
    }

    pub fn stop_live_mic(&mut self) {
        if let Some(capture) = self.mic_stream.take() {
            let (mic, rx) = capture.stop();
            self.mic = Some(mic);
            self.rx = Some(rx);
        }
    }

    /// Record 500 ms of mono PCM and replay it on both amplifier slots.
    pub fn record_and_play(&mut self) -> Result<(), Error> {
        let mic = self.mic.take().unwrap();
        let rx = self.rx.take().unwrap();
        let mut recording = match mic.read(rx) {
            Ok(transfer) => transfer,
            Err((_, mic, rx)) => {
                self.mic = Some(mic);
                self.rx = Some(rx);
                return Err(Error::CaptureStart);
            }
        };
        log::info!("Recording 500 ms; speak into the microphone");
        let start = Instant::now();
        let mut recorded = 0;
        let mut scratch = [0u8; 512];
        let mut peak = 0;
        let output = self.tx.as_mut().unwrap().as_mut_slice();
        while recorded < RECORD_BYTES && start.elapsed() < Duration::from_secs(2) {
            let wanted = scratch.len().min(RECORD_BYTES - recorded);
            let bytes = recording.pop(&mut scratch[..wanted]);
            for sample in scratch[..bytes].chunks_exact(2) {
                let sample = i16::from_le_bytes([sample[0], sample[1]]);
                peak = peak.max(sample.unsigned_abs());
                // Replay at half amplitude to keep the test quiet.
                let pcm = (sample / 2).to_le_bytes();
                output[recorded * 2..recorded * 2 + 2].copy_from_slice(&pcm);
                output[recorded * 2 + 2..recorded * 2 + 4].copy_from_slice(&pcm);
                recorded += 2;
            }
        }
        let (mic, rx) = recording.stop();
        self.mic = Some(mic);
        self.rx = Some(rx);
        if recorded != RECORD_BYTES {
            return Err(Error::CaptureTimeout(recorded));
        }
        log::info!(
            "Microphone captured {} bytes in {} ms; peak={}",
            recorded,
            start.elapsed().as_millis(),
            peak
        );
        self.tx.as_mut().unwrap().set_length(recorded * 2);
        self.play_at_mic_rate()
    }

    /// Short, low-volume 440 Hz square-wave test independent of microphone input.
    pub fn play_tone(&mut self) -> Result<(), Error> {
        let buffer = self.tx.as_mut().unwrap();
        buffer.set_length(16_000); // 250 ms of stereo 16-bit PCM
        fill_tone(&mut buffer.as_mut_slice()[..16_000], 0, MIC_SAMPLE_RATE);
        self.play_at_mic_rate()
    }

    /// Exercise streaming DMA with the same 250 ms tone used by the USB test.
    pub async fn play_stream_test_tone(&mut self) -> Result<(), Error> {
        let tone_bytes = SAMPLE_RATE as usize * STEREO_FRAME_BYTES / 4;
        self.play_pcm_stream(tone_bytes, |target, first| {
            fill_tone(target, first, SAMPLE_RATE)
        })
        .await
    }

    /// Play finite stereo PCM at SAMPLE_RATE, filling DMA progressively.
    /// The producer receives whole stereo frames and their offset in the stream.
    async fn play_pcm_stream(
        &mut self,
        bytes: usize,
        mut fill: impl FnMut(&mut [u8], usize),
    ) -> Result<(), Error> {
        let speaker = self.speaker.take().unwrap();
        let (descriptors, data) = self.stream_tx.take().unwrap().split();
        let mut buffer = DmaTxStreamBuf::new(descriptors, data).unwrap();
        let mut queued = 0;
        let mut fill_next = |target: &mut [u8]| {
            let count = target.len().min(bytes - queued);
            fill(&mut target[..count], queued / STEREO_FRAME_BYTES);
            queued += count;
            // DMA publishes complete descriptors. Pad the final descriptor with silence.
            let padded = count.next_multiple_of(STREAM_CHUNK_BYTES);
            target[count..padded].fill(0);
            padded
        };
        buffer.push_with(&mut fill_next);
        let mut playback = match speaker.write(buffer) {
            Ok(transfer) => transfer,
            Err((_, speaker, buffer)) => {
                self.speaker = Some(speaker);
                self.stream_tx = Some(buffer);
                return Err(Error::PlaybackStart);
            }
        };
        let start = Instant::now();
        let mut submitted = STREAM_BUFFER_BYTES.min(bytes);
        while !playback.is_done() && start.elapsed() < Duration::from_secs(2) {
            if submitted < bytes {
                submitted = (submitted + playback.push_with(&mut fill_next)).min(bytes);
            }
            Timer::after_micros(250).await;
        }
        let (result, speaker, buffer) = if playback.is_done() {
            let (result, speaker, buffer) = playback.wait();
            let result = result.map_err(|error| {
                log::warn!("Speaker DMA error after queuing {submitted}/{bytes} bytes: {error:?}");
                Error::PlaybackDma
            });
            let result = result.and_then(|()| {
                if submitted == bytes {
                    Ok(())
                } else {
                    log::warn!("Speaker DMA stopped after queuing {submitted}/{bytes} bytes");
                    Err(Error::PlaybackDma)
                }
            });
            (result, speaker, buffer)
        } else {
            let (speaker, buffer) = playback.stop();
            (Err(Error::PlaybackTimeout), speaker, buffer)
        };
        self.speaker = Some(speaker);
        self.stream_tx = Some(buffer);
        result
    }

    /// Prime continuous speaker DMA with whole descriptors.
    pub fn start_network_playback(
        &mut self,
        mut fill: impl FnMut(&mut [u8]) -> usize,
    ) -> Result<(), Error> {
        let (descriptors, data) = self.stream_tx.take().unwrap().split();
        let mut buffer = DmaTxStreamBuf::new(descriptors, data).unwrap();
        buffer.push_with(|target| {
            let count = fill(target);
            target[count..].fill(0);
            target.len()
        });
        let speaker = self.speaker.take().unwrap();
        match speaker.write(buffer) {
            Ok(transfer) => {
                self.network_tx = Some(transfer);
                Ok(())
            }
            Err((_, speaker, buffer)) => {
                self.speaker = Some(speaker);
                self.stream_tx = Some(buffer);
                Err(Error::PlaybackStart)
            }
        }
    }

    /// Refill only complete DMA descriptors to avoid publishing a partial sample block.
    pub fn refill_network_playback(
        &mut self,
        queued: usize,
        end: bool,
        fill: impl FnOnce(&mut [u8]) -> usize,
    ) -> bool {
        let transfer = self.network_tx.as_mut().unwrap();
        if transfer.is_done() {
            self.stop_network_playback();
            return false;
        }
        // A final short block can arrive before stream/end. Publish it before the
        // DMA chain runs out, even when the server has not sent stream/end yet.
        let draining = transfer.available_bytes() >= STREAM_BUFFER_BYTES - 2 * STREAM_CHUNK_BYTES;
        transfer.push_with(|target| {
            let available = target.len().min(queued);
            let mut count = available / STREAM_CHUNK_BYTES * STREAM_CHUNK_BYTES;
            if (end || draining) && available != 0 {
                count = available
                    .next_multiple_of(STREAM_CHUNK_BYTES)
                    .min(target.len());
            }
            if count != 0 {
                let actual = fill(&mut target[..count]);
                target[actual..count].fill(0);
            }
            count
        });
        true
    }

    pub fn stop_network_playback(&mut self) {
        if let Some(transfer) = self.network_tx.take() {
            let (speaker, buffer) = transfer.stop();
            self.speaker = Some(speaker);
            self.stream_tx = Some(buffer);
        }
    }

    fn play_at_mic_rate(&mut self) -> Result<(), Error> {
        self.set_speaker_rate(MIC_SAMPLE_RATE)?;
        let result = self.play();
        let restored = self.set_speaker_rate(SAMPLE_RATE);
        result.and(restored)
    }

    fn set_speaker_rate(&mut self, rate: u32) -> Result<(), Error> {
        self.speaker
            .as_mut()
            .unwrap()
            .apply_config(&TdmUnitConfig::new_tdm_philips().with_sample_rate(Rate::from_hz(rate)))
            .map_err(|_| Error::PlaybackConfig)
    }

    fn play(&mut self) -> Result<(), Error> {
        let speaker = self.speaker.take().unwrap();
        let tx = self.tx.take().unwrap();
        let playback = match speaker.write(tx) {
            Ok(transfer) => transfer,
            Err((_, speaker, tx)) => {
                self.speaker = Some(speaker);
                self.tx = Some(tx);
                return Err(Error::PlaybackStart);
            }
        };
        let start = Instant::now();
        while !playback.is_done() && start.elapsed() < Duration::from_secs(2) {}
        if !playback.is_done() {
            let (speaker, tx) = playback.stop();
            self.speaker = Some(speaker);
            self.tx = Some(tx);
            return Err(Error::PlaybackTimeout);
        }
        let (result, speaker, tx) = playback.wait();
        self.speaker = Some(speaker);
        self.tx = Some(tx);
        result.map_err(|_| Error::PlaybackDma)?;
        log::info!("Speaker playback complete");
        Ok(())
    }
}

fn fill_tone(target: &mut [u8], first_frame: usize, sample_rate: u32) {
    for (offset, frame) in target.chunks_exact_mut(STEREO_FRAME_BYTES).enumerate() {
        let index = first_frame + offset;
        let value: i16 = if (index * 440 * 2 / sample_rate as usize).is_multiple_of(2) {
            500
        } else {
            -500
        };
        frame[..2].copy_from_slice(&value.to_le_bytes());
        frame[2..].copy_from_slice(&value.to_le_bytes());
    }
}
