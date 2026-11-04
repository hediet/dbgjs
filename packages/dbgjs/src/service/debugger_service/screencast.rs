use super::*;
use crate::api::service_api::{
    ScreencastApi, ScreencastInput, ScreencastMessage, ScreencastOptions, ScreencastSummary,
};
use linkrpc::prelude::StreamReceiver;

#[async_trait::async_trait]
impl ScreencastApi for DebuggerService {
    async fn screencast(
        &self,
        ctx: &CallCtx,
        target_ref: TargetRef,
        options: ScreencastOptions,
        mut input: StreamReceiver<ScreencastInput>,
        output: StreamSender<ScreencastMessage>,
    ) -> Result<ScreencastSummary, JsonRpcError> {
        let _activity = self.connection_activity(&target_ref.connection).await;
        let (debugger, owned) = self.stream_target(&target_ref, "screencast").await?;
        let result = self.run_screencast(ctx, &debugger, options, &mut input, output).await;
        self.release_stream_target(&target_ref, &debugger, owned).await;
        result
    }
}

impl DebuggerService {
    /// The debugger of a target a stream shows. A discovered but unattached page is attached only
    /// for the stream, like an embedded-frame screenshot; pass the returned owner to
    /// [`Self::release_stream_target`] so the page doesn't stay attached.
    pub(super) async fn stream_target(
        &self,
        target_ref: &TargetRef,
        purpose: &str,
    ) -> Result<(TargetDebuggerHandle, Option<String>), JsonRpcError> {
        let ConnectionRef { context_id, connection_id } = &target_ref.connection;
        if let Ok(debugger) = self.target_debugger(context_id, connection_id, &target_ref.target_id).await {
            return Ok((debugger, None));
        }
        let owner_id = format!(
            "{purpose}-{}",
            random_instance_id().map_err(|error| internal_error(error.to_string()))?
        );
        let (debugger, created) = self
            .relay_ensure_attached(&owner_id, context_id, connection_id, &target_ref.target_id)
            .await?;
        Ok((debugger, created.then_some(owner_id)))
    }

    pub(super) async fn release_stream_target(
        &self,
        target_ref: &TargetRef,
        debugger: &TargetDebuggerHandle,
        owner_id: Option<String>,
    ) {
        let Some(owner_id) = owner_id else { return };
        let ConnectionRef { context_id, connection_id } = &target_ref.connection;
        let target_id = debugger.snapshot().target_id;
        self.relay_release_attachment(&owner_id, context_id, connection_id, &target_id, debugger)
            .await;
    }

    async fn run_screencast(
        &self,
        ctx: &CallCtx,
        debugger: &TargetDebuggerHandle,
        options: ScreencastOptions,
        input: &mut StreamReceiver<ScreencastInput>,
        output: StreamSender<ScreencastMessage>,
    ) -> Result<ScreencastSummary, JsonRpcError> {
        let (input_sender, input_receiver) = mpsc::channel(16);
        let (output_sender, mut output_receiver) = mpsc::channel(1);
        let run = debugger.screencast(options, input_receiver, output_sender);
        tokio::pin!(run);
        let cancelled = ctx.cancelled();
        tokio::pin!(cancelled);
        let mut input_sender = Some(input_sender);
        loop {
            tokio::select! {
                biased;
                reason = &mut cancelled => {
                    // Ending the input stops the screencast; wait for its cleanup before failing.
                    drop(input_sender.take());
                    let _ = (&mut run).await;
                    return Err(JsonRpcError::new(
                        error_codes::CANCELLED,
                        reason.unwrap_or_else(|| "screencast cancelled".to_owned()),
                    ));
                }
                result = &mut run => return result,
                message = output_receiver.recv() => {
                    if let Some(message) = message {
                        output.send(message).await?;
                    }
                }
                command = async { input.recv().await }, if input_sender.is_some() => {
                    match command {
                        Some(command) => {
                            if input_sender.as_ref().unwrap().send(command).await.is_err() {
                                input_sender = None;
                            }
                        }
                        None => input_sender = None,
                    }
                }
            }
        }
    }
}
