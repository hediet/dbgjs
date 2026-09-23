use super::*;

#[async_trait::async_trait]
impl CpuProfilerApi for DebuggerService {
    async fn start_cpu_profile(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        sampling_interval_micros: Option<u64>,
    ) -> Result<bool, CpuProfilerError> {
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
            .start_cpu_profile(sampling_interval_micros)
            .await
            .map_err(CpuProfilerError::from)?;
        Ok(true)
    }

    async fn stop_cpu_profile(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, CpuProfilerError> {
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
                    CaptureKind::CpuProfile,
                )
                .await?
        {
            let CapturePayload::CpuProfile(snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
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
                CaptureKind::CpuProfile,
            )
            .await?,
        );
        let name = reservation.reservation.metadata.name.clone();
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::CpuProfile,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::CpuProfile(snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ).into());
            };
            return Ok(snapshot);
        }
        let snapshot = match debugger
            .stop_cpu_profile(Some(name.clone()))
            .await
            .map_err(CpuProfilerError::from)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        let snapshot = debugger
            .get_cpu_profile(name.clone(), None, false, true)
            .await
            .unwrap_or(snapshot);
        self.store_capture(
            &reservation.reservation,
            CapturePayload::CpuProfile(snapshot.clone()),
        )
        .await?;
        Ok(snapshot)
    }

    async fn get_cpu_profile(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
    ) -> Result<CpuProfileSnapshot, CpuProfilerError> {
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
            .get_cpu_profile(capture_id, source_path, no_cache, project)
            .await
            .map_err(CpuProfilerError::from)
    }
}
