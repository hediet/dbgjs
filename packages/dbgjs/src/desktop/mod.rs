//! Serves a page target as an RFB (VNC) desktop over a `linkrpc.desktop.hub.v1`-style stream.

mod input;
mod rfb;

use crate::api::service_api::{
    DesktopAttachResult, DesktopClientMessage, DesktopCloseReason, DesktopLayout, DesktopOptions,
    DesktopServerMessage, ScreencastFormat, ScreencastFrameMetadata, ScreencastInput,
    ScreencastMessage, ScreencastOptions, ScreencastSize, ScreencastViewport,
};
use crate::debugger::target_debugger::TargetDebuggerHandle;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use input::InputMapper;
use linkrpc::prelude::{JsonRpcError, StreamReceiver, StreamSender, error_codes};
use rfb::{RfbInput, RfbServer};
use std::collections::VecDeque;
use std::future::Future;
use std::time::Instant;
use tokio::sync::mpsc;

/// Initial credit per direction, in bytes.
const RELAY_WINDOW: u32 = 256 * 1024;
/// Largest decoded payload of one data message.
const RELAY_CHUNK: usize = 16 * 1024;
/// No new update is encoded, and no frame acked, while this much output waits for credit.
const MAX_BACKLOG: usize = 64 * 1024;
/// Frames the screencast may have in flight to the desktop.
const FRAME_WINDOW: u8 = 2;
const MAX_VIEWPORT: u32 = 16_384;

/// Runs one desktop session until the client ends its input (`Ok`), the screencast fails, or
/// `cancelled` resolves (`Err` with `CANCELLED`).
pub async fn serve(
    debugger: TargetDebuggerHandle,
    name: String,
    options: DesktopOptions,
    input: &mut StreamReceiver<DesktopClientMessage>,
    output: StreamSender<DesktopServerMessage>,
    cancelled: impl Future<Output = Option<String>>,
) -> Result<DesktopAttachResult, JsonRpcError> {
    let (screencast_input, screencast_input_receiver) = mpsc::channel(64);
    let (screencast_output, mut frames) = mpsc::channel(4);
    let screencast = debugger.screencast(
        ScreencastOptions { format: ScreencastFormat::Png, quality: None, size: None, window: Some(FRAME_WINDOW) },
        screencast_input_receiver,
        screencast_output,
    );
    tokio::pin!(screencast);
    tokio::pin!(cancelled);
    let mut session = Session {
        rfb: RfbServer::new(name),
        mapper: InputMapper::default(),
        layout: options.layout,
        metadata: None,
        backlog: VecDeque::new(),
        credit: RELAY_WINDOW,
        deferred_acks: Vec::new(),
        screencast_input: Some(screencast_input),
        output,
    };
    session.output.send(DesktopServerMessage::Ready).await?;
    session.pump().await?;
    loop {
        tokio::select! {
            biased;
            reason = &mut cancelled => {
                // Ending the screencast's input stops it; wait for its cleanup before failing.
                session.screencast_input = None;
                let _ = (&mut screencast).await;
                return Err(JsonRpcError::new(
                    error_codes::CANCELLED,
                    reason.unwrap_or_else(|| "desktop cancelled".to_owned()),
                ));
            }
            result = &mut screencast => {
                result?;
                return Ok(DesktopAttachResult { reason: DesktopCloseReason::Closed });
            }
            message = frames.recv() => {
                if let Some(ScreencastMessage::Frame { frame_id, data_base64, metadata, .. }) = message {
                    session.frame(frame_id, &data_base64, metadata).await?;
                }
            }
            message = input.recv(), if session.screencast_input.is_some() => match message {
                Some(message) => session.client_message(message).await?,
                None => session.screencast_input = None,
            },
        }
        session.pump().await?;
    }
}

struct Session {
    rfb: RfbServer,
    mapper: InputMapper,
    layout: DesktopLayout,
    metadata: Option<ScreencastFrameMetadata>,
    /// RFB bytes not yet sent for lack of credit.
    backlog: VecDeque<u8>,
    /// Bytes the client still accepts.
    credit: u32,
    deferred_acks: Vec<u64>,
    /// `None` once the client ended its input; the screencast then stops.
    screencast_input: Option<mpsc::Sender<ScreencastInput>>,
    output: StreamSender<DesktopServerMessage>,
}

impl Session {
    async fn frame(&mut self, frame_id: u64, data_base64: &str, metadata: ScreencastFrameMetadata) -> Result<(), JsonRpcError> {
        let png = STANDARD.decode(data_base64).map_err(|error| internal(format!("screencast frame is not base64: {error}")))?;
        let (width, height, rgba) = decode_png(&png).map_err(|error| internal(format!("screencast frame is not a PNG: {error}")))?;
        self.rfb.set_frame(width, height, rgba);
        self.metadata = Some(metadata);
        if self.backlog.len() < MAX_BACKLOG {
            self.screencast(ScreencastInput::FrameAck { frame_id }).await;
        } else {
            self.deferred_acks.push(frame_id);
        }
        Ok(())
    }

    async fn client_message(&mut self, message: DesktopClientMessage) -> Result<(), JsonRpcError> {
        match message {
            DesktopClientMessage::Data { data } => {
                let bytes = STANDARD.decode(&data).map_err(|error| invalid(format!("desktop data is not base64: {error}")))?;
                if bytes.is_empty() || bytes.len() > RELAY_CHUNK || STANDARD.encode(&bytes) != data {
                    return Err(invalid("desktop data must be canonical base64 of 1..=16384 bytes"));
                }
                let inputs = self.rfb.receive(&bytes).map_err(invalid)?;
                for event in inputs {
                    self.input(event).await;
                }
                self.output.send(DesktopServerMessage::Credit { bytes: bytes.len() as u32 }).await?;
            }
            DesktopClientMessage::Credit { bytes } => {
                if bytes == 0 || self.credit + bytes > RELAY_WINDOW {
                    return Err(invalid("desktop credit exceeds the window"));
                }
                self.credit += bytes;
            }
            DesktopClientMessage::Viewport { viewport } => {
                let ratio = viewport.device_pixel_ratio;
                if !(1..=MAX_VIEWPORT).contains(&viewport.width) || !(1..=MAX_VIEWPORT).contains(&viewport.height) || !(0.5..=4.0).contains(&ratio) {
                    return Err(invalid("desktop viewport is out of range"));
                }
                let size = ScreencastSize {
                    max_width: ((f64::from(viewport.width) * ratio).round() as u32).clamp(1, MAX_VIEWPORT),
                    max_height: ((f64::from(viewport.height) * ratio).round() as u32).clamp(1, MAX_VIEWPORT),
                };
                if self.layout == DesktopLayout::Resize {
                    self.screencast(ScreencastInput::Viewport(ScreencastViewport { width: viewport.width, height: viewport.height, device_scale_factor: ratio })).await;
                }
                self.screencast(ScreencastInput::Resize(size)).await;
            }
        }
        Ok(())
    }

    async fn input(&mut self, event: RfbInput) {
        match event {
            RfbInput::Pointer { buttons, x, y } => {
                let Some(metadata) = &self.metadata else { return };
                if self.rfb.width() == 0 || self.rfb.height() == 0 {
                    return;
                }
                // Framebuffer pixels to the frame's device-independent pixels.
                let x = f64::from(x) * metadata.device_width / self.rfb.width() as f64;
                let y = f64::from(y) * metadata.device_height / self.rfb.height() as f64;
                for mouse in self.mapper.pointer(buttons, x, y, Instant::now()) {
                    self.screencast(ScreencastInput::Mouse(mouse)).await;
                }
            }
            RfbInput::Key { down, keysym, scancode } => {
                let key = self.mapper.key(down, keysym, scancode);
                self.screencast(ScreencastInput::Key(key)).await;
            }
        }
    }

    async fn screencast(&self, input: ScreencastInput) {
        if let Some(sender) = &self.screencast_input {
            // A failed send means the screencast ended; its result ends the session.
            let _ = sender.send(input).await;
        }
    }

    /// Encodes a due update, sends what credit allows, and releases deferred frame acks.
    async fn pump(&mut self) -> Result<(), JsonRpcError> {
        if self.backlog.len() < MAX_BACKLOG && self.rfb.wants_update() {
            self.rfb.write_update();
        }
        self.backlog.extend(self.rfb.take_output());
        while !self.backlog.is_empty() && self.credit > 0 {
            let len = self.backlog.len().min(RELAY_CHUNK).min(self.credit as usize);
            let chunk: Vec<u8> = self.backlog.drain(..len).collect();
            self.credit -= len as u32;
            self.output.send(DesktopServerMessage::Data { data: STANDARD.encode(chunk) }).await?;
        }
        if self.backlog.len() < MAX_BACKLOG {
            for frame_id in std::mem::take(&mut self.deferred_acks) {
                self.screencast(ScreencastInput::FrameAck { frame_id }).await;
            }
        }
        Ok(())
    }
}

/// Decodes a PNG to 8-bit RGBA.
fn decode_png(png: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).map_err(|error| error.to_string())?;
    buffer.truncate(info.buffer_size());
    let (width, height) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer.chunks_exact(3).flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buffer.chunks_exact(2).flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]]).collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&gray| [gray, gray, gray, 255]).collect(),
        png::ColorType::Indexed => return Err("indexed PNG was not expanded".into()),
    };
    Ok((width, height, rgba))
}

fn invalid(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message.into())
}

fn internal(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INTERNAL_ERROR, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgb_and_rgba_pngs_to_rgba() {
        for (color, channels) in [(png::ColorType::Rgb, 3), (png::ColorType::Rgba, 4)] {
            let mut encoded = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
                encoder.set_color(color);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                let pixels: Vec<u8> = [[10, 20, 30, 40], [50, 60, 70, 80]].iter().flat_map(|pixel| pixel[..channels].to_vec()).collect();
                writer.write_image_data(&pixels).unwrap();
            }
            let (width, height, rgba) = decode_png(&encoded).unwrap();
            assert_eq!((width, height), (2, 1));
            let alpha = |value| if channels == 4 { value } else { 255 };
            assert_eq!(rgba, vec![10, 20, 30, alpha(40), 50, 60, 70, alpha(80)]);
        }
    }
}
