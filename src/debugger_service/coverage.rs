use super::*;

enum CoveragePublicationCommand {
    Take,
    Stop,
}

impl DebuggerService {
    async fn publish_coverage(
        &self,
        target_ref: TargetRef,
        capture_id: Option<String>,
        command: CoveragePublicationCommand,
    ) -> Result<CoverageSnapshot, CoverageError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
                .promote_completed_capture(
                    &context_id,
                    &connection_id,
                    &target_id,
                    name,
                    CaptureKind::Coverage,
                )
                .await?
        {
            return coverage_from_completed(completed, name);
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture_optional(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                capture_id,
                CaptureKind::Coverage,
            )
            .await?,
        );
        let metadata = &reservation.reservation.metadata;
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &metadata.context_id,
                    &metadata.connection_id,
                    &metadata.target_id,
                    &metadata.name,
                    CaptureKind::Coverage,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            return coverage_from_completed(completed, &metadata.name);
        }
        let snapshot = match command {
            CoveragePublicationCommand::Take => {
                debugger
                    .take_coverage(Some(metadata.name.clone()))
                    .await
            }
            CoveragePublicationCommand::Stop => debugger.stop_coverage().await,
        };
        let mut snapshot = match snapshot.map_err(CoverageError::from) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        self.store_capture(
            &reservation.reservation,
            CapturePayload::Coverage(snapshot.clone()),
        )
        .await?;
        snapshot.capture_id = Some(metadata.name.clone());
        Ok(snapshot)
    }
}

fn coverage_from_completed(
    completed: PromotedCapture,
    name: &str,
) -> Result<CoverageSnapshot, CoverageError> {
    let CapturePayload::Coverage(mut snapshot) = completed.payload else {
        return Err(invalid_state("completed capture kind does not match reservation").into());
    };
    snapshot.capture_id = Some(name.to_owned());
    Ok(snapshot)
}

#[async_trait::async_trait]
impl CoverageApi for DebuggerService {
    async fn start_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
    ) -> Result<bool, CoverageError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .start_coverage()
            .await
            .map_err(CoverageError::from)?;
        Ok(true)
    }

    async fn take_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, CoverageError> {
        self.publish_coverage(
            target_ref,
            capture_id,
            CoveragePublicationCommand::Take,
        )
        .await
    }

    async fn stop_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, CoverageError> {
        self.publish_coverage(target_ref, capture_id, CoveragePublicationCommand::Stop)
            .await
    }

    async fn finish_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<bool, CoverageError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.stop_coverage(
            _ctx,
            crate::service_api::TargetRef {
                connection: crate::service_api::ConnectionRef {
                    context_id: context_id,
                    connection_id: connection_id,
                },
                target_id: target_id,
            },
            capture_id,
        )
        .await?;
        Ok(true)
    }

    async fn get_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, CoverageError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_coverage(capture_id, source_path, no_cache)
            .await
            .map_err(CoverageError::from)
    }
}
