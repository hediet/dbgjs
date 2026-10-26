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
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await;
        // A discovered but unattached page is attached only for this screencast, like an
        // embedded-frame screenshot, and released afterwards so it doesn't appear attached.
        let (debugger, owned) = match debugger {
            Ok(debugger) => (debugger, None),
            Err(_) => {
                let owner_id = format!(
                    "screencast-{}",
                    random_instance_id().map_err(|error| internal_error(error.to_string()))?
                );
                let (debugger, created) = self
                    .relay_ensure_attached(&owner_id, &context_id, &connection_id, &target_id)
                    .await?;
                (debugger, created.then_some(owner_id))
            }
        };
        let result = self.run_screencast(ctx, &debugger, options, &mut input, output).await;
        if let Some(owner_id) = owned {
            let target_id = debugger.snapshot().target_id;
            self.relay_release_attachment(&owner_id, &context_id, &connection_id, &target_id, &debugger)
                .await;
        }
        result
    }
}

impl DebuggerService {
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
