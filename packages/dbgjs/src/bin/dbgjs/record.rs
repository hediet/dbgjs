use std::io;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use dbgjs::connection::transport::session_transport::CdpSessionMux;
use dbgjs::connection::transport::websocket_transport::CdpWebSocketTransport;
use linkrpc::connection::channel::{Channel, RequestHandler};
use linkrpc::prelude::JsonRpcError;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

enum Writer {
    Async(Box<dyn AsyncWrite + Unpin + Send>),
    Stdout(mpsc::UnboundedSender<(Vec<u8>, oneshot::Sender<io::Result<()>>)>),
}

impl Writer {
    fn stdout() -> io::Result<Self> {
        let (sender, mut received) =
            mpsc::unbounded_channel::<(Vec<u8>, oneshot::Sender<io::Result<()>>)>();
        // Tokio stdout's blocking pool can prevent exit after a stalled pipe times out.
        std::thread::Builder::new()
            .name("recording-stdout".into())
            .spawn(move || {
                while let Some((bytes, done)) = received.blocking_recv() {
                    let mut stdout = io::stdout();
                    let result = io::Write::write_all(&mut stdout, &bytes)
                        .and_then(|_| io::Write::flush(&mut stdout));
                    if done.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self::Stdout(sender))
    }

    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Async(writer) => writer.write_all(bytes).await,
            Self::Stdout(sender) => {
                let (done, received) = oneshot::channel();
                sender.send((bytes.to_vec(), done)).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdout writer stopped")
                })?;
                received.await.map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdout writer stopped")
                })?
            }
        }
    }

    async fn shutdown(self) -> io::Result<()> {
        match self {
            Self::Async(mut writer) => writer.shutdown().await,
            Self::Stdout(_) => Ok(()),
        }
    }
}

pub(super) struct Options {
    output: String,
    format: Format,
    duration: Option<Duration>,
    ffmpeg: Option<PathBuf>,
    ffmpeg_args: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Mkv,
    Webm,
    Mp4,
}

impl Options {
    pub(super) fn parse(arguments: &[String]) -> io::Result<Self> {
        let mut values = std::collections::BTreeMap::new();
        let mut arguments = arguments.iter();
        while let Some(key) = arguments.next() {
            if !matches!(
                key.as_str(),
                "--output" | "--format" | "--duration-ms" | "--ffmpeg" | "--ffmpeg-args"
            ) {
                return Err(invalid(format!("unknown recording option '{key}'")));
            }
            let value = arguments
                .next()
                .ok_or_else(|| invalid(format!("{key} requires a value")))?;
            if values.insert(key.as_str(), value.as_str()).is_some() {
                return Err(invalid(format!("{key} may only be specified once")));
            }
        }
        let output = values
            .get("--output")
            .ok_or_else(|| invalid("--output is required"))?
            .to_string();
        if output.is_empty() {
            return Err(invalid("--output must not be empty"));
        }
        let extension = if output == "-" {
            None
        } else {
            Path::new(&output)
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
        };
        let explicit = values.get("--format").copied();
        let inferred = extension
            .as_deref()
            .filter(|value| matches!(*value, "mkv" | "webm" | "mp4"));
        if let (Some(format), Some(extension)) = (explicit, extension.as_deref())
            && format != extension
        {
            return Err(invalid("--format conflicts with the output file extension"));
        }
        let format = match explicit.or(inferred) {
            Some("mkv") => Format::Mkv,
            Some("webm") => Format::Webm,
            Some("mp4") => Format::Mp4,
            _ => {
                return Err(invalid(
                    "use .mkv/.webm/.mp4 or specify --format (required for stdout)",
                ));
            }
        };
        let duration = values
            .get("--duration-ms")
            .map(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .map(Duration::from_millis)
                    .ok_or_else(|| invalid("--duration-ms must be a positive integer"))
            })
            .transpose()?;
        let ffmpeg = values.get("--ffmpeg").map(PathBuf::from);
        let ffmpeg_args = values
            .get("--ffmpeg-args")
            .map(|value| {
                serde_json::from_str::<Vec<String>>(value).map_err(|error| {
                    invalid(format!(
                        "--ffmpeg-args must be a JSON array of strings: {error}"
                    ))
                })
            })
            .transpose()?;
        if let Some(arguments) = &ffmpeg_args {
            validate_encoder_args(arguments)?;
        }
        if format != Format::Mkv && ffmpeg.is_none() {
            return Err(invalid(
                "WebM/MP4 encoding requires an explicit --ffmpeg executable",
            ));
        }
        if format == Format::Mkv && (ffmpeg.is_some() || ffmpeg_args.is_some()) {
            return Err(invalid(
                "native MJPEG MKV does not use --ffmpeg or --ffmpeg-args",
            ));
        }
        Ok(Self {
            output,
            format,
            duration,
            ffmpeg,
            ffmpeg_args,
        })
    }
}

pub(super) async fn run(endpoint: &str, options: Options) -> Result<()> {
    let mut sink = Sink::open(&options).await?;
    let transport = Arc::new(CdpWebSocketTransport::connect(endpoint).await?);
    let mux = CdpSessionMux::new(transport.clone());
    let (frames, mut received) = mpsc::channel(8);
    let (failure, mut failed) = watch::channel(None);
    let channel = Channel::new(
        Box::new(mux.open_root()?),
        Box::new(Events { frames, failure }),
    );
    let mux_task = tokio::spawn({
        let mux = mux.clone();
        async move { mux.run().await }
    });
    let channel_task = tokio::spawn({
        let channel = channel.clone();
        async move { channel.run().await }
    });
    let mut started = false;
    let recording = async {
        call(&channel, "Page.enable", json!({})).await?;
        call(&channel, "Page.bringToFront", json!({})).await?;
        call(&channel, "Page.startScreencast",
            json!({"format": "jpeg", "quality": 85, "everyNthFrame": 1})).await?;
        started = true;
        // Request an initial compositor update even when the page is static.
        call(&channel, "Page.captureScreenshot", json!({"format": "jpeg"})).await?;
        eprintln!("Recording {:?}; Ctrl+C stops and finalizes the recording.", options.format);
        let stop = tokio::signal::ctrl_c();
        tokio::pin!(stop);
        let first_deadline = Instant::now() + Duration::from_secs(5);
        let mut first_time = None;
        let mut last_frame = None;
        let mut last_timestamp = 0;
        let mut count = 0_u64;
        let mut dimensions = None;
        let mut end = None;
        loop {
            let deadline = async {
                if first_time.is_none() {
                    tokio::time::sleep_until(first_deadline).await;
                } else if let Some(end) = end {
                    tokio::time::sleep_until(end).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                biased;
                signal = &mut stop => { signal?; break; }
                failure = failed.changed() => {
                    failure.map_err(|_| io::Error::other("recording event handler stopped"))?;
                    return Err(io::Error::other(failed.borrow().clone().unwrap_or_default()).into());
                }
                _ = deadline => {
                    if first_time.is_none() {
                        return Err(io::Error::other("no screencast frame within 5 seconds; keep the window visible").into());
                    }
                    break;
                }
                reason = transport.wait_closed() => {
                    return Err(io::Error::other(format!("recording relay disconnected: {reason}")).into());
                }
                event = received.recv() => {
                    let event = event.ok_or_else(|| io::Error::other("recording relay disconnected"))?;
                    let data = event.get("data").and_then(Value::as_str)
                        .ok_or_else(|| invalid("screencast frame has no data"))?;
                    let session_id = event.get("sessionId").and_then(Value::as_i64)
                        .ok_or_else(|| invalid("screencast frame has no sessionId"))?;
                    let frame = base64::engine::general_purpose::STANDARD.decode(data)?;
                    let size = jpeg_dimensions(&frame)?;
                    if dimensions.is_some_and(|dimensions| dimensions != size) {
                        return Err(invalid("renderer dimensions changed during recording; keep its size fixed").into());
                    }
                    let origin = *first_time.get_or_insert_with(Instant::now);
                    if count == 0 {
                        let (width, height) = size;
                        dimensions = Some(size);
                        sink.write(&matroska_header(width, height)).await?;
                        end = options.duration.map(|duration| origin.checked_add(duration)
                            .ok_or_else(|| invalid("--duration-ms is too large"))).transpose()?;
                    }
                    let timestamp = origin.elapsed().as_millis() as u64;
                    sink.write(&matroska_frame(timestamp, &frame)).await?;
                    last_timestamp = timestamp;
                    last_frame = Some(frame);
                    count += 1;
                    call(&channel, "Page.screencastFrameAck", json!({"sessionId": session_id})).await?;
                }
            }
        }
        let frame = last_frame.ok_or_else(|| io::Error::other("recording stopped without a frame"))?;
        let elapsed = first_time.unwrap().elapsed().as_millis() as u64;
        sink.write(&matroska_frame(elapsed.max(last_timestamp + 1), &frame)).await?;
        eprintln!("Captured {count} frames in {elapsed} ms.");
        Ok::<_, Box<dyn std::error::Error>>(())
    }.await;
    let mut cleanup = if started {
        call(&channel, "Page.stopScreencast", json!({}))
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    mux_task.abort();
    channel_task.abort();
    if timeout(Duration::from_secs(2), transport.close())
        .await
        .is_err()
    {
        let message = "recording WebSocket close exceeded 2 seconds";
        cleanup = Err(io::Error::other(match cleanup {
            Ok(()) => message.into(),
            Err(error) => format!("{error}; {message}"),
        })
        .into());
    }
    let result = match (recording, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(io::Error::other(format!("{error}; recording cleanup: {cleanup}")).into())
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => sink.finish().await,
    };
    if result.is_err() {
        if let Err(cleanup) = sink.cancel().await {
            return Err(io::Error::other(format!(
                "{}; encoder cleanup: {cleanup}",
                result.unwrap_err()
            ))
            .into());
        }
    }
    result
}

struct Events {
    frames: mpsc::Sender<Value>,
    failure: watch::Sender<Option<String>>,
}

#[async_trait]
impl RequestHandler for Events {
    async fn handle_request(
        &self,
        method: String,
        _: Value,
    ) -> std::result::Result<Value, JsonRpcError> {
        Err(JsonRpcError::new(
            -32601,
            format!("unexpected recorder request: {method}"),
        ))
    }

    async fn handle_notification(&self, method: String, params: Value) {
        if method == "Page.screencastFrame" && self.frames.try_send(params).is_err() {
            self.failure.send_replace(Some(
                "recording output cannot keep up with screencast frames".into(),
            ));
        }
    }
}

async fn call(channel: &Channel, method: &str, params: Value) -> Result<Value> {
    timeout(IO_TIMEOUT, channel.call(method, params))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{method} exceeded 10 seconds"),
            )
        })?
        .map_err(|error| io::Error::other(format!("{method}: {error:?}")).into())
}

struct Sink {
    writer: Option<Writer>,
    child: Option<Child>,
    stderr: Option<tokio::task::JoinHandle<io::Result<Vec<u8>>>>,
    staged: Option<tempfile::NamedTempFile>,
    output: String,
}

impl Sink {
    async fn open(options: &Options) -> Result<Self> {
        if options.output == "-" && io::stdout().is_terminal() {
            return Err(invalid("binary recording output requires redirected stdout").into());
        }
        let staged = if options.output == "-" {
            None
        } else {
            let destination = Path::new(&options.output);
            if destination.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recording output already exists",
                )
                .into());
            }
            let directory = destination
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Some(tempfile::NamedTempFile::new_in(directory)?)
        };
        let mut sink = Self {
            writer: None,
            child: None,
            stderr: None,
            staged,
            output: options.output.clone(),
        };
        if let Some(executable) = &options.ffmpeg {
            let mut command = Command::new(executable);
            command.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "matroska",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-fpsprobesize",
                "0",
                "-i",
                "pipe:0",
                "-an",
            ]);
            command.args(encoder_args(options));
            command.args([
                "-f",
                match options.format {
                    Format::Webm => "webm",
                    Format::Mp4 => "mp4",
                    Format::Mkv => unreachable!(),
                },
            ]);
            if options.format == Format::Mp4 && options.output == "-" {
                command.args(["-movflags", "+frag_keyframe+empty_moov+default_base_moof"]);
            }
            if let Some(staged) = &sink.staged {
                command.arg("-y").arg(staged.path()).stdout(Stdio::null());
            } else {
                command.arg("pipe:1").stdout(Stdio::inherit());
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not launch FFmpeg {}: {error}",
                        executable.display()
                    ))
                })?;
            let mut stderr = child.stderr.take().unwrap();
            sink.stderr = Some(tokio::spawn(async move {
                let mut tail = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = stderr.read(&mut buffer).await?;
                    if count == 0 {
                        return Ok(tail);
                    }
                    tail.extend_from_slice(&buffer[..count]);
                    if tail.len() > 8192 {
                        tail.drain(..tail.len() - 8192);
                    }
                }
            }));
            sink.writer = Some(Writer::Async(Box::new(child.stdin.take().unwrap())));
            sink.child = Some(child);
        } else {
            sink.writer = Some(if let Some(staged) = &sink.staged {
                Writer::Async(Box::new(tokio::fs::File::from_std(staged.reopen()?)))
            } else {
                Writer::stdout()?
            });
        }
        Ok(sink)
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        timeout(IO_TIMEOUT, self.writer.as_mut().unwrap().write_all(bytes))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "recording output blocked for 10 seconds",
                )
            })??;
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take() {
            timeout(IO_TIMEOUT, writer.shutdown()).await??;
        }
        if let Some(child) = &mut self.child {
            let status = timeout(IO_TIMEOUT, child.wait()).await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "FFmpeg finalization exceeded 10 seconds",
                )
            })??;
            let stderr = read_encoder_stderr(self.stderr.take().unwrap()).await?;
            if !status.success() {
                return Err(io::Error::other(format!(
                    "FFmpeg exited {status}: {}",
                    String::from_utf8_lossy(&stderr)
                ))
                .into());
            }
        }
        if let Some(staged) = self.staged.take() {
            if staged.as_file().metadata()?.len() == 0 {
                return Err(io::Error::other("encoder produced an empty recording").into());
            }
            staged.persist_noclobber(&self.output)?;
        }
        Ok(())
    }

    async fn cancel(&mut self) -> Result<()> {
        self.writer.take();
        if let Some(child) = &mut self.child {
            if child.try_wait()?.is_none() {
                timeout(IO_TIMEOUT, child.kill()).await.map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "FFmpeg termination exceeded 10 seconds",
                    )
                })??;
            }
            let status = child.wait().await?;
            if let Some(stderr) = self.stderr.take() {
                let stderr = read_encoder_stderr(stderr).await?;
                if !stderr.is_empty() {
                    eprintln!("FFmpeg ({status}): {}", String::from_utf8_lossy(&stderr));
                }
            }
        }
        Ok(())
    }
}

async fn read_encoder_stderr(
    mut task: tokio::task::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>> {
    match timeout(IO_TIMEOUT, &mut task).await {
        Ok(result) => Ok(result??),
        Err(_) => {
            task.abort();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "FFmpeg stderr did not close within 10 seconds",
            )
            .into())
        }
    }
}

fn encoder_args(options: &Options) -> Vec<String> {
    if let Some(arguments) = &options.ffmpeg_args {
        return arguments.clone();
    }
    let arguments: &[&str] = match options.format {
        Format::Webm => &[
            "-r",
            "25",
            "-g",
            "25",
            "-c:v",
            "vp8",
            "-deadline",
            "realtime",
            "-speed",
            "8",
            "-b:v",
            "1M",
            "-threads",
            "1",
        ],
        Format::Mp4 => &[
            "-r",
            "25",
            "-g",
            "25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
        ],
        Format::Mkv => unreachable!(),
    };
    arguments.iter().map(|value| value.to_string()).collect()
}

fn validate_encoder_args(arguments: &[String]) -> io::Result<()> {
    if arguments.len() % 2 != 0 {
        return Err(invalid(
            "--ffmpeg-args requires encoding option/value pairs",
        ));
    }
    for pair in arguments.chunks_exact(2) {
        let option = &pair[0];
        if !option.starts_with('-')
            || option == "-"
            || matches!(
                option.as_str(),
                "-i" | "-f" | "-y" | "-n" | "-stdin" | "-nostdin"
            )
        {
            return Err(invalid(format!(
                "--ffmpeg-args cannot set inputs, outputs or container options: {option}"
            )));
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn jpeg_dimensions(jpeg: &[u8]) -> io::Result<(u16, u16)> {
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return Err(invalid("invalid JPEG screencast frame"));
    }
    let mut offset = 2;
    while offset + 4 <= jpeg.len() {
        if jpeg[offset] != 0xff {
            break;
        }
        let marker = jpeg[offset + 1];
        if marker == 0xff {
            offset += 1;
            continue;
        }
        let length = u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]) as usize;
        if length < 2 || offset + 2 + length > jpeg.len() {
            break;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && length >= 7 {
            let height = u16::from_be_bytes([jpeg[offset + 5], jpeg[offset + 6]]);
            let width = u16::from_be_bytes([jpeg[offset + 7], jpeg[offset + 8]]);
            if width != 0 && height != 0 {
                return Ok((width, height));
            }
            break;
        }
        offset += length + 2;
    }
    Err(invalid("JPEG screencast frame has no valid dimensions"))
}

// Streaming Matroska: an unknown-size segment, one MJPEG track, and independently
// timestamped clusters. No index or seekable output is needed.
fn matroska_header(width: u16, height: u16) -> Vec<u8> {
    let header = element(
        0x1a45dfa3,
        [
            number(0x4286, 1),
            number(0x42f7, 1),
            number(0x42f2, 4),
            number(0x42f3, 8),
            element(0x4282, b"matroska".to_vec()),
            number(0x4287, 4),
            number(0x4285, 2),
        ]
        .concat(),
    );
    let info = element(
        0x1549a966,
        [
            number(0x2ad7b1, 1_000_000),
            element(0x4d80, b"dbgjs".to_vec()),
            element(0x5741, b"dbgjs".to_vec()),
        ]
        .concat(),
    );
    let track = element(
        0xae,
        [
            number(0xd7, 1),
            number(0x73c5, 1),
            number(0x83, 1),
            number(0x9c, 0),
            element(0x86, b"V_MJPEG".to_vec()),
            element(
                0xe0,
                [number(0xb0, width.into()), number(0xba, height.into())].concat(),
            ),
        ]
        .concat(),
    );
    [
        header,
        vec![
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
        info,
        element(0x1654ae6b, track),
    ]
    .concat()
}

fn matroska_frame(timestamp: u64, jpeg: &[u8]) -> Vec<u8> {
    element(
        0x1f43b675,
        [
            number(0xe7, timestamp),
            element(0xa3, [vec![0x81, 0, 0, 0x80], jpeg.to_vec()].concat()),
        ]
        .concat(),
    )
}

fn number(id: u32, value: u64) -> Vec<u8> {
    element(id, unsigned(value))
}

fn unsigned(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    bytes[bytes.iter().position(|byte| *byte != 0).unwrap_or(7)..].to_vec()
}

fn element(id: u32, payload: Vec<u8>) -> Vec<u8> {
    let length = payload.len() as u64;
    let size_bytes = (1..=8)
        .find(|bytes| length < (1_u64 << (7 * bytes)) - 1)
        .unwrap();
    let size = length | (1_u64 << (7 * size_bytes));
    [
        unsigned(id.into()),
        size.to_be_bytes()[8 - size_bytes..].to_vec(),
        payload,
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> io::Result<Options> {
        Options::parse(
            &arguments
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn recording_formats_are_unambiguous_and_encoders_are_external() {
        assert_eq!(
            parse(&["--output", "capture.mkv"]).unwrap().format,
            Format::Mkv
        );
        assert_eq!(
            parse(&["--output", "-", "--format", "mkv"]).unwrap().format,
            Format::Mkv
        );
        assert_eq!(
            parse(&["--output", "capture.MKV", "--format", "mkv"])
                .unwrap()
                .format,
            Format::Mkv
        );
        for arguments in [
            vec!["--output", "-"],
            vec!["--output", "a.webm"],
            vec!["--output", "a.mp4"],
            vec![
                "--output", "a.mkv", "--format", "webm", "--ffmpeg", "ffmpeg",
            ],
            vec!["--output", "a.avi", "--format", "mkv"],
            vec!["--output", "a.mkv", "--ffmpeg", "ffmpeg"],
            vec!["--output", "a.mkv", "--duration-ms", "0"],
            vec!["--output", "a.mkv", "--output", "b.mkv"],
            vec![
                "--output",
                "a.webm",
                "--ffmpeg",
                "ffmpeg",
                "--ffmpeg-args",
                "{}",
            ],
        ] {
            assert!(parse(&arguments).is_err(), "{arguments:?}");
        }
        let options = parse(&[
            "--output",
            "capture.mp4",
            "--ffmpeg",
            "custom",
            "--ffmpeg-args",
            r#"["-c:v","libx264","-crf","18"]"#,
        ])
        .unwrap();
        assert_eq!(encoder_args(&options), ["-c:v", "libx264", "-crf", "18"]);
        for arguments in [
            vec!["-i", "another.mkv"],
            vec!["another.webm", "-an"],
            vec!["-an"],
            vec!["-f", "avi"],
        ] {
            assert!(
                validate_encoder_args(
                    &arguments
                        .iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn matroska_elements_use_minimal_big_endian_sizes_and_streaming_segment() {
        assert_eq!(number(0xe7, 256), [0xe7, 0x82, 1, 0]);
        assert_eq!(&element(0xa3, vec![0; 127])[..3], [0xa3, 0x40, 0x7f]);
        let header = matroska_header(640, 480);
        assert!(header.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        assert!(header.windows(7).any(|window| window == b"V_MJPEG"));
        assert_eq!(
            matroska_frame(42, &[1, 2, 3]),
            [
                0x1f, 0x43, 0xb6, 0x75, 0x8c, 0xe7, 0x81, 42, 0xa3, 0x87, 0x81, 0, 0, 0x80, 1, 2, 3
            ]
        );
    }

    #[test]
    fn jpeg_dimensions_validate_frame_bounds() {
        assert_eq!(
            jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xc0, 0, 7, 8, 1, 0xe0, 2, 0x80]).unwrap(),
            (640, 480)
        );
        assert!(jpeg_dimensions(&[0xff, 0xd8, 0xff, 0xc0, 0xff, 0xff]).is_err());
        assert!(jpeg_dimensions(&[]).is_err());
    }
}
