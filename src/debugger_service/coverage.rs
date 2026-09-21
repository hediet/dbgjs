use super::*;

#[async_trait::async_trait]
impl CoverageApi for DebuggerService {
    async fn start_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .start_coverage()
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn take_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
        raw: Option<bool>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
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
                ));
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
                ));
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
            .take_coverage(
                Some(reservation.reservation.metadata.name.clone()),
                raw.unwrap_or(false),
            )
            .await
            .map_err(target_debugger_rpc_error)
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
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
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
                ));
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
                ));
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
            .stop_coverage()
            .await
            .map_err(target_debugger_rpc_error)
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
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError> {
        self.stop_coverage(_ctx, context_id, connection_id, target_id, capture_id)
            .await?;
        Ok(true)
    }

    async fn get_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_coverage(capture_id, source_path, no_cache)
            .await
            .map_err(target_debugger_rpc_error)
    }
}
