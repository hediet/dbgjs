//! A small RFB 3.8 server over an in-memory byte stream. The page's screencast frames are its
//! framebuffer; the client's pointer and key events come back as [`RfbInput`].
//!
//! The server is synchronous and does no I/O: feed client bytes to [`RfbServer::receive`],
//! frames to [`RfbServer::set_frame`], and send what [`RfbServer::take_output`] returns.

use flate2::{Compress, Compression};
use rfb_encodings::PixelFormat;
use rfb_encodings::tight::{SimpleTightCompressor, encode_tight_rects};
use rfb_encodings::translate::translate_pixels;

const VERSION: &[u8; 12] = b"RFB 003.008\n";
/// Changes are tracked and sent in tiles of this size.
const TILE: usize = 64;
const MAX_CUT_TEXT: usize = 1 << 20;

const ENCODING_RAW: i32 = 0;
const ENCODING_TIGHT: i32 = 7;
const ENCODING_ZRLE: i32 = 16;
const PSEUDO_DESKTOP_SIZE: i32 = -223;
const PSEUDO_DESKTOP_NAME: i32 = -307;
const PSEUDO_QEMU_EXTENDED_KEY: i32 = -258;
const PSEUDO_EXTENDED_DESKTOP_SIZE: i32 = -308;
const PSEUDO_CONTINUOUS_UPDATES: i32 = -313;
const PSEUDO_COMPRESS_LEVELS: std::ops::RangeInclusive<i32> = -256..=-247;
/// Tight quality at or above 10 disables JPEG; the page stays lossless.
const TIGHT_LOSSLESS: u8 = 255;

const SERVER_FRAMEBUFFER_UPDATE: u8 = 0;
const SERVER_END_OF_CONTINUOUS_UPDATES: u8 = 150;

/// Input decoded from the client, in framebuffer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RfbInput {
    /// `buttons` is the RFB button mask: left 1, middle 2, right 4, wheel up/down/left/right
    /// 8/16/32/64.
    Pointer { buttons: u8, x: u16, y: u16 },
    /// `scancode` is the QEMU extended key event's XT scancode (extended keys as `0x80 | code`).
    Key { down: bool, keysym: u32, scancode: Option<u32> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Version,
    SecurityType,
    ClientInit,
    /// The client is initialized, but the server sends `ServerInit` only with the first frame.
    AwaitingFrame,
    Normal,
}

pub struct RfbServer {
    name: String,
    phase: Phase,
    inbox: Vec<u8>,
    outbox: Vec<u8>,
    width: usize,
    height: usize,
    /// RGBA, `width * height * 4` bytes.
    pixels: Vec<u8>,
    dirty: Vec<bool>,
    client_format: PixelFormat,
    encoding: i32,
    compression: u8,
    desktop_size: bool,
    extended_desktop_size: bool,
    /// The client accepts name changes (DesktopName pseudo-encoding).
    desktop_name: bool,
    name_changed: bool,
    continuous: bool,
    qemu_keys: bool,
    qemu_announced: bool,
    size_changed: bool,
    update_requested: bool,
    tight: SimpleTightCompressor,
    zrle: Option<Compress>,
}

impl RfbServer {
    pub fn new(name: String) -> Self {
        Self {
            name,
            phase: Phase::Version,
            inbox: Vec::new(),
            outbox: VERSION.to_vec(),
            width: 0,
            height: 0,
            pixels: Vec::new(),
            dirty: Vec::new(),
            client_format: PixelFormat::rgba32(),
            encoding: ENCODING_RAW,
            compression: 6,
            desktop_size: false,
            extended_desktop_size: false,
            desktop_name: false,
            name_changed: false,
            continuous: false,
            qemu_keys: false,
            qemu_announced: false,
            size_changed: false,
            update_requested: false,
            tight: SimpleTightCompressor::new(6),
            zrle: None,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Bytes for the client, oldest first.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outbox)
    }

    /// Consumes client bytes; returns the input events they completed.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<RfbInput>, String> {
        self.inbox.extend_from_slice(bytes);
        let inbox = std::mem::take(&mut self.inbox);
        let mut inputs = Vec::new();
        let mut offset = 0;
        let result = loop {
            match self.parse(&inbox[offset..], &mut inputs) {
                Ok(Some(consumed)) => offset += consumed,
                Ok(None) => break Ok(inputs),
                Err(error) => break Err(error),
            }
        };
        self.inbox = inbox[offset..].to_vec();
        result
    }

    /// Replaces the framebuffer with an RGBA frame and marks the tiles that changed.
    pub fn set_frame(&mut self, width: usize, height: usize, rgba: Vec<u8>) {
        assert_eq!(rgba.len(), width * height * 4, "frame size mismatch");
        if width != self.width || height != self.height {
            let resized = !self.pixels.is_empty();
            self.width = width;
            self.height = height;
            self.pixels = rgba;
            self.dirty = vec![true; self.tiles_x() * self.tiles_y()];
            self.size_changed = resized;
        } else {
            self.mark_changed_tiles(&rgba);
            self.pixels = rgba;
        }
        if self.phase == Phase::AwaitingFrame {
            self.write_server_init();
        }
    }

    /// Renames the desktop; clients that support it learn the new name with the next update.
    pub fn set_name(&mut self, name: String) {
        if name != self.name {
            self.name = name;
            self.name_changed = self.desktop_name;
        }
    }

    /// Whether an update is due: requested (or continuous) and something to send.
    pub fn wants_update(&self) -> bool {
        self.phase == Phase::Normal
            && (self.continuous || self.update_requested)
            && (self.size_changed
                || self.name_changed
                || (self.qemu_keys && !self.qemu_announced)
                || self.dirty.iter().any(|dirty| *dirty))
    }

    /// Appends a framebuffer update with every pending change to the output.
    pub fn write_update(&mut self) {
        let mut body = Vec::new();
        let mut rects = 0usize;
        if self.qemu_keys && !self.qemu_announced {
            put_rect_header(&mut body, 0, 0, 0, 0, PSEUDO_QEMU_EXTENDED_KEY);
            rects += 1;
            self.qemu_announced = true;
        }
        if self.size_changed {
            rects += self.write_size_announcement(&mut body);
            self.size_changed = false;
        }
        if self.name_changed {
            put_rect_header(&mut body, 0, 0, 0, 0, PSEUDO_DESKTOP_NAME);
            body.extend_from_slice(&(self.name.len() as u32).to_be_bytes());
            body.extend_from_slice(self.name.as_bytes());
            rects += 1;
            self.name_changed = false;
        }
        for (x, y, width, height) in self.dirty_spans() {
            rects += self.encode_rect(x, y, width, height, &mut body);
        }
        self.dirty.iter_mut().for_each(|dirty| *dirty = false);
        self.update_requested = false;
        self.outbox.push(SERVER_FRAMEBUFFER_UPDATE);
        self.outbox.push(0);
        self.outbox.extend_from_slice(&u16::try_from(rects).unwrap_or(u16::MAX).to_be_bytes());
        self.outbox.extend_from_slice(&body);
    }

    /// Parses one message from `input`; `None` when it is incomplete.
    fn parse(&mut self, input: &[u8], inputs: &mut Vec<RfbInput>) -> Result<Option<usize>, String> {
        match self.phase {
            Phase::Version => {
                let Some(version) = input.get(..12) else { return Ok(None) };
                if version != VERSION && version != b"RFB 003.007\n" {
                    return Err(format!("unsupported RFB version {:?}", String::from_utf8_lossy(version)));
                }
                // One security type: none. Hub consent already authorized the call.
                self.outbox.extend_from_slice(&[1, 1]);
                self.phase = Phase::SecurityType;
                Ok(Some(12))
            }
            Phase::SecurityType => {
                let Some(&kind) = input.first() else { return Ok(None) };
                if kind != 1 {
                    return Err(format!("unsupported RFB security type {kind}"));
                }
                self.outbox.extend_from_slice(&0u32.to_be_bytes());
                self.phase = Phase::ClientInit;
                Ok(Some(1))
            }
            Phase::ClientInit => {
                if input.is_empty() {
                    return Ok(None);
                }
                self.phase = Phase::AwaitingFrame;
                if !self.pixels.is_empty() {
                    self.write_server_init();
                }
                Ok(Some(1))
            }
            Phase::AwaitingFrame | Phase::Normal => self.parse_client_message(input, inputs),
        }
    }

    fn parse_client_message(&mut self, input: &[u8], inputs: &mut Vec<RfbInput>) -> Result<Option<usize>, String> {
        let Some(&kind) = input.first() else { return Ok(None) };
        let needs = |len: usize| input.len() >= len;
        match kind {
            0 => {
                if !needs(20) {
                    return Ok(None);
                }
                self.set_pixel_format(&input[4..20])?;
                Ok(Some(20))
            }
            2 => {
                if !needs(4) {
                    return Ok(None);
                }
                let len = 4 + usize::from(u16_at(input, 2)) * 4;
                if !needs(len) {
                    return Ok(None);
                }
                let encodings: Vec<i32> = input[4..len].chunks_exact(4).map(|bytes| i32::from_be_bytes(bytes.try_into().unwrap())).collect();
                self.set_encodings(&encodings);
                Ok(Some(len))
            }
            3 => {
                if !needs(10) {
                    return Ok(None);
                }
                if input[1] == 0 {
                    self.dirty.iter_mut().for_each(|dirty| *dirty = true);
                }
                self.update_requested = true;
                Ok(Some(10))
            }
            4 => {
                if !needs(8) {
                    return Ok(None);
                }
                inputs.push(RfbInput::Key { down: input[1] != 0, keysym: u32_at(input, 4), scancode: None });
                Ok(Some(8))
            }
            5 => {
                if !needs(6) {
                    return Ok(None);
                }
                inputs.push(RfbInput::Pointer { buttons: input[1], x: u16_at(input, 2), y: u16_at(input, 4) });
                Ok(Some(6))
            }
            6 => {
                if !needs(8) {
                    return Ok(None);
                }
                let len = u32_at(input, 4) as usize;
                if len > MAX_CUT_TEXT {
                    return Err(format!("RFB clipboard text of {len} bytes is too large"));
                }
                Ok(needs(8 + len).then_some(8 + len))
            }
            150 => {
                if !needs(10) {
                    return Ok(None);
                }
                self.continuous = input[1] != 0;
                if self.continuous {
                    self.dirty.iter_mut().for_each(|dirty| *dirty = true);
                } else {
                    self.outbox.push(SERVER_END_OF_CONTINUOUS_UPDATES);
                }
                Ok(Some(10))
            }
            248 => {
                if !needs(9) {
                    return Ok(None);
                }
                let len = 9 + usize::from(input[8]);
                Ok(needs(len).then_some(len))
            }
            251 => {
                if !needs(8) {
                    return Ok(None);
                }
                // The page follows the viewport sent over the stream instead.
                let len = 8 + usize::from(input[6]) * 16;
                Ok(needs(len).then_some(len))
            }
            255 => {
                if !needs(2) {
                    return Ok(None);
                }
                if input[1] != 0 {
                    return Err(format!("unsupported QEMU client message {}", input[1]));
                }
                if !needs(12) {
                    return Ok(None);
                }
                let scancode = u32_at(input, 8);
                inputs.push(RfbInput::Key {
                    down: u16_at(input, 2) != 0,
                    keysym: u32_at(input, 4),
                    scancode: (scancode != 0).then_some(scancode),
                });
                Ok(Some(12))
            }
            other => Err(format!("unsupported RFB client message {other}")),
        }
    }

    fn set_pixel_format(&mut self, bytes: &[u8]) -> Result<(), String> {
        let format = PixelFormat {
            bits_per_pixel: bytes[0],
            depth: bytes[1],
            big_endian_flag: bytes[2],
            true_colour_flag: bytes[3],
            red_max: u16_at(bytes, 4),
            green_max: u16_at(bytes, 6),
            blue_max: u16_at(bytes, 8),
            red_shift: bytes[10],
            green_shift: bytes[11],
            blue_shift: bytes[12],
        };
        if format.true_colour_flag == 0 || ![8, 16, 32].contains(&format.bits_per_pixel) {
            return Err("RFB colour maps are not supported".into());
        }
        self.client_format = format;
        self.dirty.iter_mut().for_each(|dirty| *dirty = true);
        Ok(())
    }

    fn set_encodings(&mut self, encodings: &[i32]) {
        self.encoding = encodings
            .iter()
            .copied()
            .find(|encoding| [ENCODING_TIGHT, ENCODING_ZRLE, ENCODING_RAW].contains(encoding))
            .unwrap_or(ENCODING_RAW);
        self.desktop_size = encodings.contains(&PSEUDO_DESKTOP_SIZE);
        self.extended_desktop_size = encodings.contains(&PSEUDO_EXTENDED_DESKTOP_SIZE);
        self.desktop_name = encodings.contains(&PSEUDO_DESKTOP_NAME);
        self.qemu_keys = encodings.contains(&PSEUDO_QEMU_EXTENDED_KEY);
        if let Some(level) = encodings.iter().find(|encoding| PSEUDO_COMPRESS_LEVELS.contains(encoding)) {
            self.compression = u8::try_from(level - PSEUDO_COMPRESS_LEVELS.start()).unwrap_or(6);
            self.tight = SimpleTightCompressor::new(self.compression);
            self.zrle = None;
        }
        if encodings.contains(&PSEUDO_CONTINUOUS_UPDATES) {
            // Confirms the extension; the client may then enable continuous updates.
            self.outbox.push(SERVER_END_OF_CONTINUOUS_UPDATES);
        }
    }

    fn write_server_init(&mut self) {
        let format = PixelFormat::rgba32();
        self.outbox.extend_from_slice(&(self.width as u16).to_be_bytes());
        self.outbox.extend_from_slice(&(self.height as u16).to_be_bytes());
        self.outbox.extend_from_slice(&[format.bits_per_pixel, format.depth, format.big_endian_flag, format.true_colour_flag]);
        for max in [format.red_max, format.green_max, format.blue_max] {
            self.outbox.extend_from_slice(&max.to_be_bytes());
        }
        self.outbox.extend_from_slice(&[format.red_shift, format.green_shift, format.blue_shift, 0, 0, 0]);
        self.outbox.extend_from_slice(&(self.name.len() as u32).to_be_bytes());
        self.outbox.extend_from_slice(self.name.as_bytes());
        self.phase = Phase::Normal;
        self.size_changed = false;
    }

    fn write_size_announcement(&self, body: &mut Vec<u8>) -> usize {
        let (width, height) = (self.width as u16, self.height as u16);
        if self.extended_desktop_size {
            // Reason 0 (server-side change), status 0, one screen covering the framebuffer.
            put_rect_header(body, 0, 0, width, height, PSEUDO_EXTENDED_DESKTOP_SIZE);
            body.extend_from_slice(&[1, 0, 0, 0]);
            body.extend_from_slice(&0u32.to_be_bytes());
            for value in [0, 0, width, height] {
                body.extend_from_slice(&value.to_be_bytes());
            }
            body.extend_from_slice(&0u32.to_be_bytes());
            1
        } else if self.desktop_size {
            put_rect_header(body, 0, 0, width, height, PSEUDO_DESKTOP_SIZE);
            1
        } else {
            0
        }
    }

    fn tiles_x(&self) -> usize {
        self.width.div_ceil(TILE)
    }

    fn tiles_y(&self) -> usize {
        self.height.div_ceil(TILE)
    }

    fn mark_changed_tiles(&mut self, rgba: &[u8]) {
        let stride = self.width * 4;
        let tiles_x = self.tiles_x();
        for tile_y in 0..self.tiles_y() {
            for tile_x in 0..tiles_x {
                let index = tile_y * tiles_x + tile_x;
                if self.dirty[index] {
                    continue;
                }
                let columns = tile_x * TILE * 4..((tile_x + 1) * TILE).min(self.width) * 4;
                self.dirty[index] = (tile_y * TILE..((tile_y + 1) * TILE).min(self.height)).any(|row| {
                    let range = row * stride + columns.start..row * stride + columns.end;
                    rgba[range.clone()] != self.pixels[range]
                });
            }
        }
    }

    /// Dirty tiles merged into rectangles: horizontal runs per tile row, then equal runs stacked.
    fn dirty_spans(&self) -> Vec<(usize, usize, usize, usize)> {
        let tiles_x = self.tiles_x();
        let mut spans: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for tile_y in 0..self.tiles_y() {
            let y = tile_y * TILE;
            let height = TILE.min(self.height - y);
            let mut next_open = Vec::new();
            let mut tile_x = 0;
            while tile_x < tiles_x {
                if !self.dirty[tile_y * tiles_x + tile_x] {
                    tile_x += 1;
                    continue;
                }
                let start = tile_x;
                while tile_x < tiles_x && self.dirty[tile_y * tiles_x + tile_x] {
                    tile_x += 1;
                }
                let x = start * TILE;
                let width = (tile_x * TILE).min(self.width) - x;
                let stacked = open.iter().copied().find(|&index| {
                    let (span_x, span_y, span_width, span_height) = spans[index];
                    span_x == x && span_width == width && span_y + span_height == y
                });
                if let Some(index) = stacked {
                    spans[index].3 += height;
                    next_open.push(index);
                } else {
                    spans.push((x, y, width, height));
                    next_open.push(spans.len() - 1);
                }
            }
            open = next_open;
        }
        spans
    }

    /// Encodes one framebuffer rectangle; returns how many RFB rectangles it took.
    fn encode_rect(&mut self, x: usize, y: usize, width: usize, height: usize, body: &mut Vec<u8>) -> usize {
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in y..y + height {
            let start = (row * self.width + x) * 4;
            rgba.extend_from_slice(&self.pixels[start..start + width * 4]);
        }
        let (x16, y16, width16, height16) = (x as u16, y as u16, width as u16, height as u16);
        match self.encoding {
            ENCODING_TIGHT => {
                let rects = encode_tight_rects(&rgba, width16, height16, TIGHT_LOSSLESS, self.compression, &self.client_format, &mut self.tight);
                for (rect_x, rect_y, rect_width, rect_height, data) in &rects {
                    put_rect_header(body, x16 + rect_x, y16 + rect_y, *rect_width, *rect_height, ENCODING_TIGHT);
                    body.extend_from_slice(data);
                }
                rects.len()
            }
            ENCODING_ZRLE => {
                let pixels = translate_pixels(&rgba, &PixelFormat::rgba32(), &self.client_format);
                let compression = u32::from(self.compression);
                let zrle = self.zrle.get_or_insert_with(|| Compress::new(Compression::new(compression), true));
                match rfb_encodings::encode_zrle_persistent(&pixels, width16, height16, &self.client_format, zrle) {
                    Ok(data) => {
                        put_rect_header(body, x16, y16, width16, height16, ENCODING_ZRLE);
                        body.extend_from_slice(&data);
                    }
                    Err(_) => {
                        put_rect_header(body, x16, y16, width16, height16, ENCODING_RAW);
                        body.extend_from_slice(&pixels);
                    }
                }
                1
            }
            _ => {
                put_rect_header(body, x16, y16, width16, height16, ENCODING_RAW);
                body.extend_from_slice(&translate_pixels(&rgba, &PixelFormat::rgba32(), &self.client_format));
                1
            }
        }
    }
}

fn put_rect_header(body: &mut Vec<u8>, x: u16, y: u16, width: u16, height: u16, encoding: i32) {
    for value in [x, y, width, height] {
        body.extend_from_slice(&value.to_be_bytes());
    }
    body.extend_from_slice(&encoding.to_be_bytes());
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(server: &mut RfbServer) {
        assert_eq!(server.take_output(), VERSION.to_vec());
        server.receive(VERSION).unwrap();
        assert_eq!(server.take_output(), vec![1, 1]);
        server.receive(&[1]).unwrap();
        assert_eq!(server.take_output(), vec![0, 0, 0, 0]);
        server.receive(&[1]).unwrap();
    }

    fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat(width * height)
    }

    fn set_encodings(encodings: &[i32]) -> Vec<u8> {
        let mut message = vec![2, 0];
        message.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for encoding in encodings {
            message.extend_from_slice(&encoding.to_be_bytes());
        }
        message
    }

    /// Splits an update without compressed rectangles into `(x, y, width, height, encoding)`.
    fn update_rects(update: &[u8]) -> Vec<(u16, u16, u16, u16, i32)> {
        assert_eq!(update[0], SERVER_FRAMEBUFFER_UPDATE);
        let mut offset = 4;
        let mut rects = Vec::new();
        for _ in 0..u16_at(update, 2) {
            let encoding = i32::from_be_bytes(update[offset + 8..offset + 12].try_into().unwrap());
            let rect = (u16_at(update, offset), u16_at(update, offset + 2), u16_at(update, offset + 4), u16_at(update, offset + 6), encoding);
            offset += 12;
            match encoding {
                ENCODING_RAW => offset += usize::from(rect.2) * usize::from(rect.3) * 4,
                PSEUDO_EXTENDED_DESKTOP_SIZE => offset += 20,
                PSEUDO_DESKTOP_NAME => offset += 4 + u32_at(update, offset) as usize,
                _ => {}
            }
            rects.push(rect);
        }
        assert_eq!(offset, update.len(), "update length");
        rects
    }

    #[test]
    fn server_init_waits_for_the_first_frame_and_describes_it() {
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        assert!(server.take_output().is_empty());
        server.set_frame(2, 1, solid(2, 1, [1, 2, 3, 255]));
        let init = server.take_output();
        assert_eq!(&init[..4], &[0, 2, 0, 1]);
        assert_eq!(&init[4..8], &[32, 24, 0, 1]);
        assert_eq!(&init[20..24], &4u32.to_be_bytes());
        assert_eq!(&init[24..], b"page");
    }

    #[test]
    fn incremental_updates_send_only_changed_tiles() {
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        server.set_frame(130, 70, solid(130, 70, [0, 0, 0, 255]));
        server.take_output();
        server.receive(&set_encodings(&[ENCODING_RAW])).unwrap();
        server.receive(&[3, 0, 0, 0, 0, 0, 0, 130, 0, 70]).unwrap();
        assert!(server.wants_update());
        server.write_update();
        assert_eq!(update_rects(&server.take_output()), vec![(0, 0, 130, 70, 0)]);

        let mut changed = solid(130, 70, [0, 0, 0, 255]);
        changed[(65 * 130 + 129) * 4] = 9;
        server.set_frame(130, 70, changed);
        assert!(!server.wants_update(), "updates wait for a request");
        server.receive(&[3, 1, 0, 0, 0, 0, 0, 130, 0, 70]).unwrap();
        server.write_update();
        assert_eq!(update_rects(&server.take_output()), vec![(128, 64, 2, 6, 0)]);
    }

    #[test]
    fn continuous_updates_are_confirmed_and_announce_resizes() {
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        server.set_frame(4, 4, solid(4, 4, [0, 0, 0, 255]));
        server.take_output();
        server.receive(&set_encodings(&[ENCODING_RAW, PSEUDO_EXTENDED_DESKTOP_SIZE, PSEUDO_CONTINUOUS_UPDATES, PSEUDO_QEMU_EXTENDED_KEY])).unwrap();
        assert_eq!(server.take_output(), vec![SERVER_END_OF_CONTINUOUS_UPDATES]);
        server.receive(&[150, 1, 0, 0, 0, 0, 0, 4, 0, 4]).unwrap();
        server.write_update();
        assert_eq!(update_rects(&server.take_output()), vec![(0, 0, 0, 0, PSEUDO_QEMU_EXTENDED_KEY), (0, 0, 4, 4, 0)]);
        assert!(!server.wants_update());

        server.set_frame(2, 3, solid(2, 3, [5, 5, 5, 255]));
        assert!(server.wants_update(), "continuous updates need no request");
        server.write_update();
        assert_eq!(update_rects(&server.take_output()), vec![(0, 0, 2, 3, PSEUDO_EXTENDED_DESKTOP_SIZE), (0, 0, 2, 3, 0)]);
    }

    #[test]
    fn renames_reach_clients_that_accept_desktop_names() {
        let mut server = RfbServer::new("old".into());
        handshake(&mut server);
        server.set_frame(4, 4, solid(4, 4, [0, 0, 0, 255]));
        server.take_output();
        server.receive(&set_encodings(&[ENCODING_RAW, PSEUDO_DESKTOP_NAME])).unwrap();
        server.receive(&[3, 1, 0, 0, 0, 0, 0, 4, 0, 4]).unwrap();
        server.write_update();
        server.take_output();
        server.receive(&[3, 1, 0, 0, 0, 0, 0, 4, 0, 4]).unwrap();
        server.set_name("old".into());
        assert!(!server.wants_update(), "an unchanged name is no update");
        server.set_name("Agents · new".into());
        assert!(server.wants_update());
        server.write_update();
        let update = server.take_output();
        assert_eq!(update_rects(&update), vec![(0, 0, 0, 0, PSEUDO_DESKTOP_NAME)]);
        assert_eq!(&update[20..], "Agents · new".as_bytes());

        let mut plain = RfbServer::new("old".into());
        handshake(&mut plain);
        plain.set_frame(4, 4, solid(4, 4, [0, 0, 0, 255]));
        plain.take_output();
        plain.receive(&[3, 1, 0, 0, 0, 0, 0, 4, 0, 4]).unwrap();
        plain.write_update();
        plain.take_output();
        plain.receive(&[3, 1, 0, 0, 0, 0, 0, 4, 0, 4]).unwrap();
        plain.set_name("new".into());
        assert!(!plain.wants_update(), "clients without the pseudo-encoding keep the initial name");
    }

    #[test]
    fn decodes_pointer_and_extended_key_events_across_chunks() {
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        let mut messages = vec![5, 1, 0, 10, 0, 20];
        messages.extend_from_slice(&[255, 0, 0, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x1e]);
        messages.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0xff, 0x0d]);
        let (first, second) = messages.split_at(9);
        assert_eq!(server.receive(first).unwrap(), vec![RfbInput::Pointer { buttons: 1, x: 10, y: 20 }]);
        assert_eq!(server.receive(second).unwrap(), vec![
            RfbInput::Key { down: true, keysym: 0x61, scancode: Some(0x1e) },
            RfbInput::Key { down: false, keysym: 0xff0d, scancode: None },
        ]);
    }

    #[test]
    fn encodes_updates_with_tight_and_zrle() {
        for encoding in [ENCODING_TIGHT, ENCODING_ZRLE] {
            let mut server = RfbServer::new("page".into());
            handshake(&mut server);
            let mut frame = solid(100, 80, [10, 20, 30, 255]);
            frame[..400].fill(200);
            server.set_frame(100, 80, frame);
            server.take_output();
            server.receive(&set_encodings(&[encoding])).unwrap();
            server.receive(&[3, 0, 0, 0, 0, 0, 0, 100, 0, 80]).unwrap();
            server.write_update();
            let update = server.take_output();
            assert_eq!(update[0], SERVER_FRAMEBUFFER_UPDATE);
            assert!(u16_at(&update, 2) >= 1);
            assert_eq!(i32::from_be_bytes(update[12..16].try_into().unwrap()), encoding);
            assert!(update.len() < 100 * 80, "encoding {encoding} took {} bytes", update.len());
        }
    }

    #[test]
    fn rejects_unknown_messages_and_colour_maps() {
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        assert!(server.receive(&[7]).is_err());
        let mut server = RfbServer::new("page".into());
        handshake(&mut server);
        let mut set_pixel_format = vec![0, 0, 0, 0, 8, 8, 0, 0];
        set_pixel_format.extend_from_slice(&[0; 12]);
        assert!(server.receive(&set_pixel_format).is_err());
    }
}
