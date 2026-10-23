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
            .await?;
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
