use super::*;
use crate::api::service_api::{
    ScreencastFormat, ScreencastFrameMetadata, ScreencastInput, ScreencastMessage,
    ScreencastOptions, ScreencastSize, ScreencastSummary, ScreencastViewport,
};
use linkrpc::prelude::{JsonRpcError, error_codes};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

const INPUT_QUEUE: usize = 256;
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const FIRST_FRAME_KICK_TIMEOUT: Duration = Duration::from_secs(5);

impl TargetDebuggerHandle {
    /// Runs one screencast of this target until `input` ends (`Ok`) or the target fails (`Err`).
    /// Frames go to `output`; the browser produces the next frame only after the client acks
    /// the pending one. Pointer and key input is dispatched in order on a separate task, so a
    /// blocking input dispatch (e.g. a native context menu) does not stall frames.
    pub async fn screencast(
        &self,
        options: ScreencastOptions,
        mut input: mpsc::Receiver<ScreencastInput>,
        output: mpsc::Sender<ScreencastMessage>,
    ) -> Result<ScreencastSummary, JsonRpcError> {
        let mut events = self.raw_events.subscribe();
        self.raw_cdp_request("Page.enable".into(), json!({})).await?;
        self.start_screencast(&options, options.size).await?;
        let kick = self.clone();
        let first_frame_kick = tokio::spawn(async move {
            // Forces a compositor frame so an idle page still produces its first screencast frame.
            let _ = tokio::time::timeout(
                FIRST_FRAME_KICK_TIMEOUT,
                kick.raw_cdp_request("Page.captureScreenshot".into(), json!({ "format": "jpeg", "quality": 1 })),
            ).await;
        });
        let (dispatch, dispatch_queue) = mpsc::channel(INPUT_QUEUE);
        // Set whenever the page zoom may have changed; the dispatcher then re-reads it before the next mouse event.
        let zoom_stale = Arc::new(AtomicBool::new(true));
        let mut dispatcher = tokio::spawn(dispatch_input(self.clone(), dispatch_queue, zoom_stale.clone()));
        let mut frames = 0u64;
        let mut pending_frame = None;
        let mut viewport_overridden = false;
        let result = loop {
            tokio::select! {
                biased;
                failure = &mut dispatcher => break Err(match failure {
                    Ok(Err(error)) => error,
                    Ok(Ok(())) => internal_error("screencast input dispatcher stopped"),
                    Err(error) => internal_error(&format!("screencast input dispatcher failed: {error}")),
                }),
                message = input.recv() => match message {
                    None => break Ok(()),
                    Some(ScreencastInput::FrameAck { frame_id }) => {
                        if pending_frame == Some(frame_id) {
                            pending_frame = None;
                            if let Err(error) = self.raw_cdp_request(
                                "Page.screencastFrameAck".into(),
                                json!({ "sessionId": frame_id }),
                            ).await {
                                break Err(error);
                            }
                        }
                    }
                    Some(ScreencastInput::Resize(size)) => {
                        pending_frame = None;
                        if let Err(error) = self.raw_cdp_request("Page.stopScreencast".into(), json!({})).await {
                            break Err(error);
                        }
                        if let Err(error) = self.start_screencast(&options, Some(size)).await {
                            break Err(error);
                        }
                    }
                    Some(ScreencastInput::Viewport(viewport)) => {
                        if let Err(error) = self.override_viewport(viewport).await {
                            break Err(error);
                        }
                        viewport_overridden = true;
                        zoom_stale.store(true, Ordering::SeqCst);
                    }
                    Some(ScreencastInput::ResetViewport) => {
                        if viewport_overridden {
                            if let Err(error) = self.raw_cdp_request("Emulation.clearDeviceMetricsOverride".into(), json!({})).await {
                                break Err(error);
                            }
                            viewport_overridden = false;
                            zoom_stale.store(true, Ordering::SeqCst);
                        }
                    }
                    Some(command) => {
                        if dispatch.try_send(command).is_err() {
                            break Err(invalid_state("screencast input queue overflow; input is arriving faster than the page handles it"));
                        }
                    }
                },
                event = events.recv() => match event {
                    Ok(event) if event.session.session_id != self.session_id => {}
                    Ok(event) => match event.method.as_str() {
                        "Page.screencastFrame" => {
                            let frame = match parse_frame(&options, &event.params) {
                                Ok(frame) => frame,
                                Err(error) => break Err(error),
                            };
                            if let ScreencastMessage::Frame { frame_id, .. } = &frame {
                                pending_frame = Some(*frame_id);
                            }
                            // A zoom change repaints, so every zoom change is followed by a frame.
                            zoom_stale.store(true, Ordering::SeqCst);
                            frames += 1;
                            if output.send(frame).await.is_err() {
                                break Ok(());
                            }
                        }
                        "Page.screencastVisibilityChanged" => {
                            let visible = event.params.get("visible").and_then(Value::as_bool).unwrap_or(true);
                            if output.send(ScreencastMessage::Visibility { visible }).await.is_err() {
                                break Ok(());
                            }
                        }
                        "Inspector.detached" => {
                            break Err(JsonRpcError::new(error_codes::PEER_DISCONNECTED, "target detached during screencast"));
                        }
                        _ => {}
                    },
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        break Err(invalid_state(&format!("screencast lost {count} target events; restart the screencast")));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break Err(JsonRpcError::new(error_codes::PEER_DISCONNECTED, "target debugger stopped during screencast"));
                    }
                },
            }
        };
        dispatcher.abort();
        first_frame_kick.abort();
        if viewport_overridden {
            let _ = tokio::time::timeout(
                STOP_TIMEOUT,
                self.raw_cdp_request("Emulation.clearDeviceMetricsOverride".into(), json!({})),
            ).await;
        }
        let _ = tokio::time::timeout(
            STOP_TIMEOUT,
            self.raw_cdp_request("Page.stopScreencast".into(), json!({})),
        ).await;
        result.map(|()| ScreencastSummary { frames })
    }

    async fn start_screencast(
        &self,
        options: &ScreencastOptions,
        size: Option<ScreencastSize>,
    ) -> Result<(), JsonRpcError> {
        let mut params = json!({
            "format": match options.format {
                ScreencastFormat::Jpeg => "jpeg",
                ScreencastFormat::Png => "png",
            },
            "everyNthFrame": 1,
        });
        if let Some(quality) = options.quality {
            if quality > 100 {
                return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, "quality must be in 0..=100"));
            }
            params["quality"] = json!(quality);
        }
        if let Some(size) = size {
            if size.max_width == 0 || size.max_height == 0 {
                return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, "screencast size must be positive"));
            }
            params["maxWidth"] = json!(size.max_width);
            params["maxHeight"] = json!(size.max_height);
        }
        self.raw_cdp_request("Page.startScreencast".into(), params).await.map(drop)
    }

    async fn override_viewport(&self, viewport: ScreencastViewport) -> Result<(), JsonRpcError> {
        if viewport.width == 0 || viewport.height == 0 || viewport.width > 16_384 || viewport.height > 16_384 {
            return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, "viewport size must be in 1..=16384"));
        }
        if !(viewport.device_scale_factor > 0.0 && viewport.device_scale_factor <= 8.0) {
            return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, "viewport deviceScaleFactor must be in (0, 8]"));
        }
        self.raw_cdp_request("Emulation.setDeviceMetricsOverride".into(), json!({
            "width": viewport.width,
            "height": viewport.height,
            "deviceScaleFactor": viewport.device_scale_factor,
            "mobile": false,
        })).await.map(drop)
    }
}

async fn dispatch_input(
    target: TargetDebuggerHandle,
    mut queue: mpsc::Receiver<ScreencastInput>,
    zoom_stale: Arc<AtomicBool>,
) -> Result<(), JsonRpcError> {
    let mut zoom = 1.0;
    while let Some(command) = queue.recv().await {
        let (method, params) = match command {
            ScreencastInput::Mouse(mut mouse) => {
                if zoom_stale.swap(false, Ordering::SeqCst) {
                    zoom = page_zoom(&target).await?;
                }
                // Frame coordinates are device-independent pixels; Input.dispatchMouseEvent takes CSS pixels.
                mouse.x = to_css_pixels(mouse.x, zoom);
                mouse.y = to_css_pixels(mouse.y, zoom);
                ("Input.dispatchMouseEvent", serde_json::to_value(mouse))
            }
            ScreencastInput::Key(key) => ("Input.dispatchKeyEvent", serde_json::to_value(key)),
            ScreencastInput::FrameAck { .. }
            | ScreencastInput::Resize(_)
            | ScreencastInput::Viewport(_)
            | ScreencastInput::ResetViewport => continue,
        };
        let params = strip_nulls(params.map_err(|error| internal_error(&error.to_string()))?);
        target.raw_cdp_request(method.into(), params).await?;
    }
    // The screencast loop owns the sender; it ending means the loop already finished.
    std::future::pending().await
}

/// The page's CSS-to-device-independent pixel ratio (e.g. 1.2 for a page zoomed to 120%).
async fn page_zoom(target: &TargetDebuggerHandle) -> Result<f64, JsonRpcError> {
    let metrics = target.raw_cdp_request("Page.getLayoutMetrics".into(), json!({})).await?;
    // Chromium before 92 omits `zoom`; such pages were never zoomed for input purposes.
    Ok(metrics.pointer("/cssVisualViewport/zoom")
        .and_then(Value::as_f64)
        .filter(|zoom| zoom.is_finite() && *zoom > 0.0)
        .unwrap_or(1.0))
}

/// Rounded to 1/1000 px: Chromium reports zoom as a float32 (1.2 is 1.2000000477), so 960 / zoom
/// would be 799.99997, which the page's integer `clientX` truncates to 799.
fn to_css_pixels(frame_pixels: f64, zoom: f64) -> f64 {
    (frame_pixels / zoom * 1000.0).round() / 1000.0
}

/// CDP rejects explicit `null` for optional input fields.
fn strip_nulls(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, value| !value.is_null());
    }
    value
}

fn parse_frame(options: &ScreencastOptions, params: &Value) -> Result<ScreencastMessage, JsonRpcError> {
    let malformed = |field: &str| invalid_state(&format!("malformed Page.screencastFrame: missing {field}"));
    let frame_id = params.get("sessionId").and_then(Value::as_u64).ok_or_else(|| malformed("sessionId"))?;
    let data_base64 = params.get("data").and_then(Value::as_str).ok_or_else(|| malformed("data"))?.to_owned();
    let metadata: ScreencastFrameMetadata = serde_json::from_value(
        params.get("metadata").cloned().ok_or_else(|| malformed("metadata"))?,
    ).map_err(|error| invalid_state(&format!("malformed Page.screencastFrame metadata: {error}")))?;
    Ok(ScreencastMessage::Frame {
        frame_id,
        media_type: match options.format {
            ScreencastFormat::Jpeg => "image/jpeg",
            ScreencastFormat::Png => "image/png",
        }.to_owned(),
        data_base64,
        metadata,
    })
}

fn invalid_state(message: &str) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_REQUEST, message)
}

fn internal_error(message: &str) -> JsonRpcError {
    JsonRpcError::new(error_codes::INTERNAL_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::service_api::{
        ScreencastKeyEventType, ScreencastKeyInput, ScreencastMouseButton, ScreencastMouseEventType,
        ScreencastMouseInput, TargetDebuggerPhase,
    };
    use crate::debugger::cdp_runtime::RawCdpEvent;
    use crate::debugger::debugger_engine::SessionKey;
    use linkrpc::connection::channel::{Channel, RejectingHandler};
    use linkrpc::prelude::{JsonRpcMessage, MessageTransport};
    use linkrpc::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse, ResponsePayload};
    use linkrpc::transport::memory::transport_pair_of;

    struct FakeBrowser<T: MessageTransport> {
        transport: T,
    }

    impl<T: MessageTransport> FakeBrowser<T> {
        /// Receives the next CDP request and answers it with `{}`.
        async fn answer(&self) -> (String, Value) {
            self.answer_with(json!({})).await
        }

        /// Receives the next CDP request and answers it with `result`.
        async fn answer_with(&self, result: Value) -> (String, Value) {
            let JsonRpcMessage::Request(JsonRpcRequest { id, method, params }) =
                tokio::time::timeout(Duration::from_secs(2), self.transport.recv())
                    .await
                    .expect("expected a CDP request")
                    .unwrap()
            else {
                panic!("expected a CDP request");
            };
            self.transport.send(JsonRpcMessage::Response(JsonRpcResponse {
                id: Some(id),
                payload: ResponsePayload::Result(result),
            })).await.unwrap();
            (method, params.unwrap_or(Value::Null))
        }
    }

    fn event(session_id: &str, method: &str, params: Value) -> RawCdpEvent {
        RawCdpEvent {
            session: SessionKey { connection_generation: 1, session_id: session_id.into() },
            method: method.into(),
            params,
        }
    }

    fn frame_event(session_id: &str, frame_id: u64) -> RawCdpEvent {
        event(session_id, "Page.screencastFrame", json!({
            "sessionId": frame_id,
            "data": "AAAA",
            "metadata": {
                "offsetTop": 0, "pageScaleFactor": 1, "deviceWidth": 800, "deviceHeight": 600,
                "scrollOffsetX": 0, "scrollOffsetY": 0, "timestamp": 1.5,
            },
        }))
    }

    #[tokio::test]
    async fn screencast_forwards_own_frames_acks_them_dispatches_input_and_stops() {
        let (transport, browser) = transport_pair_of();
        let browser = FakeBrowser { transport: browser };
        let channel = Channel::new(Box::new(transport), Box::new(RejectingHandler));
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        let mut handle = TargetDebuggerHandle::stub_for_tests(
            super::super::tests::target_snapshot(TargetDebuggerPhase::Running),
        );
        let (raw_events, _) = broadcast::channel(16);
        handle.raw_events = raw_events.clone();
        handle.raw_cdp_channel = Some(channel);
        let (commands, _command_receiver) = mpsc::channel(1);
        handle.commands = commands;

        let (input, input_receiver) = mpsc::channel(8);
        let (output, mut frames) = mpsc::channel(8);
        let options = ScreencastOptions {
            format: ScreencastFormat::Jpeg,
            quality: Some(70),
            size: Some(ScreencastSize { max_width: 800, max_height: 600 }),
        };
        let run = tokio::spawn({
            let handle = handle.clone();
            async move { handle.screencast(options, input_receiver, output).await }
        });

        assert_eq!(browser.answer().await, ("Page.enable".into(), json!({})));
        assert_eq!(browser.answer().await, ("Page.startScreencast".into(), json!({
            "format": "jpeg", "everyNthFrame": 1, "quality": 70, "maxWidth": 800, "maxHeight": 600,
        })));
        assert_eq!(browser.answer().await.0, "Page.captureScreenshot");

        raw_events.send(frame_event("child-session", 6)).unwrap();
        raw_events.send(frame_event("native-session", 7)).unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv()).await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&frame).unwrap(), json!({
            "kind": "frame", "frameId": 7, "mediaType": "image/jpeg", "dataBase64": "AAAA",
            "metadata": {
                "offsetTop": 0.0, "pageScaleFactor": 1.0, "deviceWidth": 800.0, "deviceHeight": 600.0,
                "scrollOffsetX": 0.0, "scrollOffsetY": 0.0, "timestamp": 1.5,
            },
        }));

        input.send(ScreencastInput::FrameAck { frame_id: 6 }).await.unwrap();
        input.send(ScreencastInput::FrameAck { frame_id: 7 }).await.unwrap();
        assert_eq!(browser.answer().await, ("Page.screencastFrameAck".into(), json!({ "sessionId": 7 })));

        input.send(ScreencastInput::Mouse(ScreencastMouseInput {
            event_type: ScreencastMouseEventType::MousePressed,
            x: 10.0, y: 20.0,
            button: ScreencastMouseButton::Left,
            buttons: 1, click_count: 1, delta_x: 0.0, delta_y: 0.0, modifiers: 0,
        })).await.unwrap();
        assert_eq!(browser.answer().await.0, "Page.getLayoutMetrics");
        assert_eq!(browser.answer().await, ("Input.dispatchMouseEvent".into(), json!({
            "type": "mousePressed", "x": 10.0, "y": 20.0, "button": "left", "buttons": 1,
            "clickCount": 1, "deltaX": 0.0, "deltaY": 0.0, "modifiers": 0,
        })));
        input.send(ScreencastInput::Key(ScreencastKeyInput {
            event_type: ScreencastKeyEventType::KeyDown,
            key: "a".into(), code: "KeyA".into(), text: Some("a".into()),
            windows_virtual_key_code: Some(65), auto_repeat: false, modifiers: 0,
        })).await.unwrap();
        assert_eq!(browser.answer().await, ("Input.dispatchKeyEvent".into(), json!({
            "type": "keyDown", "key": "a", "code": "KeyA", "text": "a",
            "windowsVirtualKeyCode": 65, "autoRepeat": false, "modifiers": 0,
        })));

        drop(input);
        assert_eq!(browser.answer().await.0, "Page.stopScreencast");
        assert_eq!(run.await.unwrap().unwrap(), ScreencastSummary { frames: 1 });
        channel_task.abort();
    }

    #[tokio::test]
    async fn screencast_removes_its_viewport_override_when_the_stream_ends() {
        let (transport, browser) = transport_pair_of();
        let browser = FakeBrowser { transport: browser };
        let channel = Channel::new(Box::new(transport), Box::new(RejectingHandler));
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        let mut handle = TargetDebuggerHandle::stub_for_tests(
            super::super::tests::target_snapshot(TargetDebuggerPhase::Running),
        );
        let (raw_events, _) = broadcast::channel(16);
        handle.raw_events = raw_events;
        handle.raw_cdp_channel = Some(channel);
        let (commands, _command_receiver) = mpsc::channel(1);
        handle.commands = commands;

        let (input, input_receiver) = mpsc::channel(8);
        let (output, _frames) = mpsc::channel(8);
        let run = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle.screencast(
                    ScreencastOptions { format: ScreencastFormat::Jpeg, quality: None, size: None },
                    input_receiver,
                    output,
                ).await
            }
        });
        browser.answer().await;
        browser.answer().await;
        browser.answer().await;
        input.send(ScreencastInput::Viewport(ScreencastViewport { width: 800, height: 600, device_scale_factor: 1.5 })).await.unwrap();
        assert_eq!(browser.answer().await, ("Emulation.setDeviceMetricsOverride".into(), json!({
            "width": 800, "height": 600, "deviceScaleFactor": 1.5, "mobile": false,
        })));
        drop(input);
        assert_eq!(browser.answer().await.0, "Emulation.clearDeviceMetricsOverride");
        assert_eq!(browser.answer().await.0, "Page.stopScreencast");
        assert_eq!(run.await.unwrap().unwrap(), ScreencastSummary { frames: 0 });
        channel_task.abort();
    }

    #[tokio::test]
    async fn screencast_converts_mouse_input_to_css_pixels_of_a_zoomed_page() {
        let (transport, browser) = transport_pair_of();
        let browser = FakeBrowser { transport: browser };
        let channel = Channel::new(Box::new(transport), Box::new(RejectingHandler));
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        let mut handle = TargetDebuggerHandle::stub_for_tests(
            super::super::tests::target_snapshot(TargetDebuggerPhase::Running),
        );
        let (raw_events, _) = broadcast::channel(16);
        handle.raw_events = raw_events.clone();
        handle.raw_cdp_channel = Some(channel);
        let (commands, _command_receiver) = mpsc::channel(1);
        handle.commands = commands;

        let (input, input_receiver) = mpsc::channel(8);
        let (output, mut frames) = mpsc::channel(8);
        let run = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle.screencast(
                    ScreencastOptions { format: ScreencastFormat::Jpeg, quality: None, size: None },
                    input_receiver,
                    output,
                ).await
            }
        });
        browser.answer().await;
        browser.answer().await;
        browser.answer().await;
        let mouse_move = |x, y| ScreencastInput::Mouse(ScreencastMouseInput {
            event_type: ScreencastMouseEventType::MouseMoved,
            x, y,
            button: ScreencastMouseButton::None,
            buttons: 0, click_count: 0, delta_x: 0.0, delta_y: 0.0, modifiers: 0,
        });
        let dispatched_at = |(method, params): (String, Value)| {
            assert_eq!(method, "Input.dispatchMouseEvent");
            (params["x"].as_f64().unwrap(), params["y"].as_f64().unwrap())
        };

        // A page zoomed to 120% (reported as float32): frame (960, 600) is CSS (800, 500).
        input.send(mouse_move(960.0, 600.0)).await.unwrap();
        assert_eq!(
            browser.answer_with(json!({ "cssVisualViewport": { "zoom": 1.2000000476837158 } })).await.0,
            "Page.getLayoutMetrics",
        );
        assert_eq!(dispatched_at(browser.answer().await), (800.0, 500.0));

        // The zoom is cached until the next frame.
        input.send(mouse_move(120.0, 60.0)).await.unwrap();
        assert_eq!(dispatched_at(browser.answer().await), (100.0, 50.0));

        // A frame may follow a zoom change, so the zoom is read again.
        raw_events.send(frame_event("native-session", 1)).unwrap();
        tokio::time::timeout(Duration::from_secs(2), frames.recv()).await.unwrap().unwrap();
        input.send(mouse_move(480.0, 360.0)).await.unwrap();
        assert_eq!(
            browser.answer_with(json!({ "cssVisualViewport": { "zoom": 1.0 } })).await.0,
            "Page.getLayoutMetrics",
        );
        assert_eq!(dispatched_at(browser.answer().await), (480.0, 360.0));

        drop(input);
        assert_eq!(browser.answer().await.0, "Page.stopScreencast");
        run.await.unwrap().unwrap();
        channel_task.abort();
    }

    #[tokio::test]
    async fn screencast_fails_explicitly_when_the_target_detaches() {
        let (transport, browser) = transport_pair_of();
        let browser = FakeBrowser { transport: browser };
        let channel = Channel::new(Box::new(transport), Box::new(RejectingHandler));
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        let mut handle = TargetDebuggerHandle::stub_for_tests(
            super::super::tests::target_snapshot(TargetDebuggerPhase::Running),
        );
        let (raw_events, _) = broadcast::channel(16);
        handle.raw_events = raw_events.clone();
        handle.raw_cdp_channel = Some(channel);
        let (commands, _command_receiver) = mpsc::channel(1);
        handle.commands = commands;

        let (_input, input_receiver) = mpsc::channel(8);
        let (output, _frames) = mpsc::channel(8);
        let run = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle.screencast(
                    ScreencastOptions { format: ScreencastFormat::Png, quality: None, size: None },
                    input_receiver,
                    output,
                ).await
            }
        });
        browser.answer().await;
        browser.answer().await;
        browser.answer().await;
        raw_events.send(event("native-session", "Inspector.detached", json!({ "reason": "target_closed" }))).unwrap();
        assert_eq!(browser.answer().await.0, "Page.stopScreencast");
        let error = run.await.unwrap().unwrap_err();
        assert_eq!((error.code, error.message.as_str()), (error_codes::PEER_DISCONNECTED, "target detached during screencast"));
        channel_task.abort();
    }
}
