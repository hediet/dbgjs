use super::*;

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
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
            snapshot.capture_id = Some(name.clone());
            return Ok(snapshot);
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
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::Coverage,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
            .take_coverage(Some(reservation.reservation.metadata.name.clone()))
            .await
            .map_err(CoverageError::from)
        {
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
        snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
        Ok(snapshot)
    }

    async fn stop_coverage(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
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
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
            snapshot.capture_id = Some(name.clone());
            return Ok(snapshot);
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
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::Coverage,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
            .stop_coverage()
            .await
            .map_err(CoverageError::from)
        {
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
        snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
        Ok(snapshot)
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
