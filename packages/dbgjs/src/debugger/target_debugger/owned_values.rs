use super::*;
use crate::api::value::DescribedProperty;
use oxc_allocator::Allocator;
use oxc_ast::ast::{ArrowFunctionExpression, AwaitExpression, Function};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::{Value, json};

const CAPACITY: usize = 1024;
const EXECUTION_TIMEOUT_MS: u64 = 1000;

#[derive(Clone)]
struct OwnedValue {
    remote: RuntimeRemoteObject,
    group: String,
    continuation: bool,
    unboxed: bool,
    await_result: bool,
    completed: Option<(RuntimeRemoteObject, Option<String>)>,
    preview_truncated: bool,
    pause_epoch: Option<u64>,
    handled: bool,
    container_id: Option<String>,
}

#[derive(Default)]
pub(super) struct OwnedValues {
    entries: BTreeMap<String, OwnedValue>,
    generation: u64,
}

fn failure(message: impl Into<String>) -> TargetDebuggerError {
    TargetDebuggerError::Evaluation(message.into())
}

fn new_reference(continuation: bool) -> Result<String, TargetDebuggerError> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| failure(e.to_string()))?;
    Ok(format!(
        "@{}{}",
        if continuation { 'e' } else { 'v' },
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

/// Parse as a module so top-level await is legal, but never descend into function bodies.
fn has_top_level_await(expression: &str) -> Result<bool, TargetDebuggerError> {
    let allocator = Allocator::default();
    let source = format!("({expression}\n)");
    let parsed = Parser::new(&allocator, &source, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        return Err(failure(format!("invalid expression: {}", parsed.errors[0])));
    }
    #[derive(Default)]
    struct Detector(bool);
    impl<'a> Visit<'a> for Detector {
        fn visit_await_expression(&mut self, _: &AwaitExpression<'a>) {
            self.0 = true;
        }
        fn visit_function(&mut self, _: &Function<'a>, _: oxc_syntax::scope::ScopeFlags) {}
        fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
    }
    let mut detector = Detector::default();
    walk::walk_program(&mut detector, &parsed.program);
    Ok(detector.0)
}

async fn request(
    driver: &DebuggerDriver,
    method: &str,
    params: Value,
) -> Result<Value, TargetDebuggerError> {
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        driver.raw_cdp_request(method, params),
    )
    .await
    .map_err(|_| {
        failure(format!(
            "{method} inspection timed out; target execution was not cancelled"
        ))
    })?
    .map_err(|e| failure(format!("value is stale or unavailable: {}", e.message)))?;
    if let Some(exception) = response.get("exceptionDetails") {
        return Err(failure(
            exception
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exception.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation failed"),
        ));
    }
    Ok(response)
}

async fn descriptors(
    driver: &DebuggerDriver,
    id: &str,
) -> Result<
    (
        Vec<RuntimePropertyDescriptor>,
        Vec<RuntimeInternalPropertyDescriptor>,
    ),
    TargetDebuggerError,
> {
    let result = request(
        driver,
        "Runtime.getProperties",
        json!({
            "objectId": id, "ownProperties": true, "generatePreview": false
        }),
    )
    .await?;
    Ok((
        serde_json::from_value(result["result"].clone()).map_err(|e| failure(e.to_string()))?,
        serde_json::from_value(
            result
                .get("internalProperties")
                .cloned()
                .unwrap_or(json!([])),
        )
        .map_err(|e| failure(e.to_string()))?,
    ))
}

async fn mark_handled(
    driver: &DebuggerDriver,
    remote: &RuntimeRemoteObject,
    group: &str,
) -> Result<(), TargetDebuggerError> {
    if remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise) {
        request(driver, "Runtime.callFunctionOn", json!({
            "objectId": remote.object_id, "objectGroup": group,
            "functionDeclaration": "function(){ Promise.prototype.then.call(this, undefined, () => {}); }",
            "returnByValue": true, "silent": true
        })).await?;
    }
    Ok(())
}

fn promise_parts(
    internal: &[RuntimeInternalPropertyDescriptor],
) -> (String, Option<RuntimeRemoteObject>) {
    let (state, result) = crate::debugger::promise_debugging::live_promise_parts(internal);
    (
        serialized_enum_name(&state).unwrap_or_else(|| "unknown".into()),
        result.cloned(),
    )
}

impl OwnedValues {
    pub(super) async fn invalidate(&mut self, driver: &DebuggerDriver) {
        if self.generation != driver.value_generation() {
            self.release_all(driver).await;
            self.generation = driver.value_generation();
        }
    }
    pub(super) async fn release_all(&mut self, driver: &DebuggerDriver) {
        let groups = std::mem::take(&mut self.entries)
            .into_values()
            .map(|v| v.group)
            .collect::<BTreeSet<_>>();
        for group in groups {
            release_group(driver, group).await;
        }
    }

    pub(super) async fn execute(
        &mut self,
        driver: &mut DebuggerDriver,
        session: &SessionKey,
        operation: ValueOperation,
        options: &DescribeOptions,
    ) -> Result<ValueDescription, TargetDebuggerError> {
        self.invalidate(driver).await;
        if options.max_nodes == 0
            || options.max_properties == 0
            || options.max_string_length == 0
            || options.max_depth > 64
        {
            return Err(failure(
                "inspection limits must be positive and max-depth must not exceed 64",
            ));
        }
        let pause = driver
            .state()
            .sessions
            .get(session)
            .and_then(|s| s.pause.as_ref())
            .map(|p| p.epoch);
        let expired = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.pause_epoch.is_some() && entry.pause_epoch != pause)
            .map(|(reference, _)| reference.clone())
            .collect::<Vec<_>>();
        for reference in expired {
            let entry = self.entries.remove(&reference).unwrap();
            if !self.entries.values().any(|v| v.group == entry.group) {
                release_group(driver, entry.group).await;
            }
        }
        match operation {
            ValueOperation::Evaluate {
                expression,
                retain,
                await_result,
            } => {
                if self.entries.len() >= CAPACITY {
                    return Err(failure(
                        "retained value capacity (1024) reached; release a value first",
                    ));
                }
                let continuation = has_top_level_await(&expression)?;
                if continuation && pause.is_some() {
                    return Err(failure(
                        "top-level await cannot progress while the target is paused; resume explicitly",
                    ));
                }
                let reference = new_reference(continuation)?;
                let group = format!("dbgjs-value-{reference}");
                let acquired = async {
                    let mut preview_truncated = false;
                    let mut container_id = None;
                    let remote = if continuation {
                        let response = request(driver, "Runtime.evaluate", json!({
                            "expression": format!("((p) => {{ Promise.prototype.then.call(p, undefined, () => {{}}); return p; }})((async () => ({{value: (\n{expression}\n)}}))())"),
                            "objectGroup": group, "returnByValue": false, "generatePreview": false,
                            "awaitPromise": false, "timeout": EXECUTION_TIMEOUT_MS
                        })).await?;
                        let remote = serde_json::from_value(response["result"].clone()).map_err(|e| failure(e.to_string()))?;
                        remote
                    } else {
                        let expression = if await_result {
                            // Observe a rejected native promise before Node's next unhandled-rejection turn.
                            format!("((v) => {{ if (v instanceof Promise) Promise.prototype.then.call(v, undefined, () => {{}}); return v; }})(\n{expression}\n)")
                        } else { expression };
                        let evaluated = evaluate_remote(driver, session, pause, 0, expression, true, options.max_string_length, Some(&group), Some(EXECUTION_TIMEOUT_MS)).await?;
                        preview_truncated = evaluated.preview_truncated;
                        container_id = evaluated.container_id;
                        evaluated.remote
                    };
                    Ok::<_, TargetDebuggerError>((remote, preview_truncated, container_id))
                }.await;
                let (remote, preview_truncated, container_id) = match acquired {
                    Ok(remote) => remote,
                    Err(error) => {
                        release_group(driver, group).await;
                        return Err(error);
                    }
                };
                let retained = retain
                    || continuation
                    || remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise);
                let entry = OwnedValue {
                    remote,
                    group: group.clone(),
                    continuation,
                    unboxed: false,
                    await_result,
                    completed: None,
                    preview_truncated,
                    pause_epoch: pause,
                    handled: continuation || await_result,
                    container_id,
                };
                if retained {
                    self.entries.insert(reference.clone(), entry.clone());
                }
                let mut result = if continuation
                    || await_result
                        && entry.remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise)
                {
                    self.await_entry(
                        driver,
                        session,
                        &reference,
                        &entry,
                        options,
                        pause.is_some(),
                    )
                    .await
                } else {
                    describe(
                        driver,
                        session,
                        entry.remote.clone(),
                        options,
                        retained.then_some(reference.clone()),
                    )
                    .await
                };
                if let Ok(value) = &mut result {
                    value.truncated |= preview_truncated;
                }
                let finished_without_reference = !retain
                    && result.as_ref().is_ok_and(|value| {
                        value.state.as_deref() != Some("pending")
                            && value.reference.as_deref() != Some(reference.as_str())
                    });
                if !retained || result.is_err() || finished_without_reference {
                    self.entries.remove(&reference);
                    if !self.entries.values().any(|v| v.group == group) {
                        release_group(driver, group).await;
                    }
                }
                result
            }
            operation => {
                let reference = match &operation {
                    ValueOperation::Show { reference }
                    | ValueOperation::Children { reference }
                    | ValueOperation::Await { reference }
                    | ValueOperation::Release { reference } => reference,
                    _ => unreachable!(),
                }
                .clone();
                let mut entry = self.entries.get(&reference).cloned().ok_or_else(|| failure("unknown, released, or stale value reference (references belong to one target incarnation and pause)"))?;
                if matches!(operation, ValueOperation::Release { .. }) {
                    self.entries.remove(&reference);
                    if !self.entries.values().any(|v| v.group == entry.group) {
                        driver
                            .client()
                            .runtime()
                            .release_object_group(entry.group)
                            .await
                            .map_err(|e| failure(format!("{e:?}")))?;
                    }
                    let mut value = ValueDescription::new("released");
                    value.summary = Some(format!("Released {reference}"));
                    return Ok(value);
                }
                if entry.preview_truncated
                    && let Some(container) = &entry.container_id
                {
                    let projected = project_evaluation_container(
                        driver,
                        session,
                        pause,
                        container.clone(),
                        options.max_string_length,
                        Some(&entry.group),
                    )
                    .await?;
                    entry.remote = projected.remote;
                    entry.preview_truncated = projected.preview_truncated;
                }
                let mut result = if matches!(operation, ValueOperation::Await { .. }) {
                    self.await_entry(
                        driver,
                        session,
                        &reference,
                        &entry,
                        options,
                        pause.is_some(),
                    )
                    .await
                } else if let Some((remote, result_reference)) = &entry.completed
                    && entry.continuation
                {
                    describe(
                        driver,
                        session,
                        remote.clone(),
                        options,
                        result_reference.clone(),
                    )
                    .await
                } else if entry.continuation {
                    self.await_entry(
                        driver,
                        session,
                        &reference,
                        &entry,
                        options,
                        pause.is_some(),
                    )
                    .await
                } else {
                    describe(
                        driver,
                        session,
                        entry.remote.clone(),
                        options,
                        Some(reference.clone()),
                    )
                    .await
                };
                if let Ok(value) = &mut result {
                    value.truncated |= entry.preview_truncated;
                }
                if matches!(operation, ValueOperation::Children { .. }) {
                    let remote = if entry.continuation {
                        self.entries
                            .get(&reference)
                            .and_then(|entry| entry.completed.as_ref())
                            .map(|(remote, _)| remote)
                            .unwrap_or(&entry.remote)
                    } else {
                        &entry.remote
                    };
                    if let (Ok(value), Some(id)) = (&mut result, remote.object_id.as_deref()) {
                        let (properties, _) = descriptors(driver, id).await?;
                        let children = value
                            .properties
                            .iter()
                            .filter_map(|child| {
                                properties
                                    .iter()
                                    .find(|p| p.name == child.name)
                                    .and_then(|p| p.value.clone())
                                    .filter(|v| v.object_id.is_some())
                                    .map(|remote| (child.name.clone(), remote))
                            })
                            .collect::<Vec<_>>();
                        if self.entries.len() + children.len() > CAPACITY {
                            return Err(failure(
                                "retained value capacity (1024) reached; release values before expanding children",
                            ));
                        }
                        for (name, remote) in children {
                            let child_reference = new_reference(false)?;
                            self.entries.insert(
                                child_reference.clone(),
                                OwnedValue {
                                    remote,
                                    group: entry.group.clone(),
                                    continuation: false,
                                    unboxed: false,
                                    await_result: false,
                                    completed: None,
                                    preview_truncated: false,
                                    pause_epoch: entry.pause_epoch,
                                    handled: false,
                                    container_id: None,
                                },
                            );
                            value
                                .properties
                                .iter_mut()
                                .find(|p| p.name == name)
                                .unwrap()
                                .value
                                .reference = Some(child_reference);
                        }
                    }
                }
                if result
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains("stale"))
                {
                    self.entries.remove(&reference);
                    if !self.entries.values().any(|v| v.group == entry.group) {
                        release_group(driver, entry.group).await;
                    }
                }
                result
            }
        }
    }

    async fn await_entry(
        &mut self,
        driver: &mut DebuggerDriver,
        session: &SessionKey,
        reference: &str,
        entry: &OwnedValue,
        options: &DescribeOptions,
        paused: bool,
    ) -> Result<ValueDescription, TargetDebuggerError> {
        if let Some((remote, result_reference)) = &entry.completed {
            if result_reference
                .as_ref()
                .is_some_and(|r| !self.entries.contains_key(r))
            {
                return self
                    .complete(driver, session, reference, entry, remote.clone(), options)
                    .await;
            }
            return describe(
                driver,
                session,
                remote.clone(),
                options,
                result_reference.clone(),
            )
            .await;
        }
        if entry.remote.subtype != Some(RuntimeRemoteObjectSubtype::Promise) {
            return Err(failure(
                "value await requires a promise or evaluation continuation",
            ));
        }
        let id = entry
            .remote
            .object_id
            .as_deref()
            .ok_or_else(|| failure("promise has no object handle"))?;
        let (_, internal) = descriptors(driver, id).await?;
        let (state, result) = promise_parts(&internal);
        if state == "unknown" {
            return Err(failure("promise settlement state is unavailable"));
        }
        if !entry.handled {
            mark_handled(driver, &entry.remote, &entry.group).await?;
            self.entries.get_mut(reference).unwrap().handled = true;
        }
        if state == "pending" {
            if paused {
                return Err(failure(
                    "pending await cannot progress while target is paused; resume explicitly",
                ));
            }
            let mut value = ValueDescription::new(if entry.continuation {
                "evaluation"
            } else {
                "promise"
            });
            value.state = Some("pending".into());
            value.reference = Some(reference.into());
            return Ok(value);
        }
        let remote = result.ok_or_else(|| failure("promise settlement is unavailable"))?;
        if state == "rejected" {
            let reason = describe(driver, session, remote, options, None).await?;
            let mut value = ValueDescription::new("rejection");
            value.state = Some(state);
            value.result = Some(Box::new(reason));
            value.reference = Some(reference.into());
            return Ok(value);
        }
        if !entry.continuation || entry.unboxed {
            return self
                .complete(driver, session, reference, entry, remote, options)
                .await;
        }
        let box_id = remote
            .object_id
            .as_deref()
            .ok_or_else(|| failure("evaluation result box is unavailable"))?;
        let (properties, _) = descriptors(driver, box_id).await?;
        let remote = properties
            .into_iter()
            .find(|p| p.name == "value")
            .and_then(|p| p.value)
            .ok_or_else(|| failure("evaluation result box has no value"))?;
        let result = if remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise) {
            if self.entries.len() >= CAPACITY {
                return Err(failure("retained value capacity (1024) reached"));
            }
            let next_reference = new_reference(false)?;
            let next = OwnedValue {
                remote: remote.clone(),
                group: entry.group.clone(),
                continuation: false,
                unboxed: false,
                await_result: false,
                completed: None,
                preview_truncated: false,
                pause_epoch: entry.pause_epoch,
                handled: false,
                container_id: None,
            };
            self.entries.insert(next_reference.clone(), next.clone());
            if entry.await_result {
                let result = Box::pin(self.await_entry(
                    driver,
                    session,
                    &next_reference,
                    &next,
                    options,
                    paused,
                ))
                .await?;
                // Keep the operation intention on the original continuation through subsequent polls.
                if result.state.as_deref() == Some("pending") {
                    let stored = self.entries.get_mut(reference).unwrap();
                    stored.remote = remote;
                    stored.unboxed = true;
                    stored.handled = true;
                    self.entries.remove(&next_reference);
                    let mut result = result;
                    result.kind = "evaluation".into();
                    result.reference = Some(reference.into());
                    return Ok(result);
                }
                if let Some(completed) = self
                    .entries
                    .get(&next_reference)
                    .and_then(|v| v.completed.clone())
                {
                    self.entries.get_mut(reference).unwrap().completed = Some(completed);
                }
                self.entries.remove(&next_reference);
                result
            } else {
                self.entries.get_mut(reference).unwrap().completed =
                    Some((remote.clone(), Some(next_reference.clone())));
                describe(driver, session, remote, options, Some(next_reference)).await?
            }
        } else {
            self.complete(driver, session, reference, entry, remote, options)
                .await?
        };
        Ok(result)
    }

    async fn complete(
        &mut self,
        driver: &mut DebuggerDriver,
        session: &SessionKey,
        reference: &str,
        entry: &OwnedValue,
        remote: RuntimeRemoteObject,
        options: &DescribeOptions,
    ) -> Result<ValueDescription, TargetDebuggerError> {
        let result_reference = if remote.object_id.is_some() {
            if self.entries.len() >= CAPACITY {
                return Err(failure(
                    "retained value capacity (1024) reached; release a value first",
                ));
            }
            let result_reference = new_reference(false)?;
            self.entries.insert(
                result_reference.clone(),
                OwnedValue {
                    remote: remote.clone(),
                    group: entry.group.clone(),
                    continuation: false,
                    unboxed: false,
                    await_result: false,
                    completed: None,
                    preview_truncated: false,
                    pause_epoch: entry.pause_epoch,
                    handled: false,
                    container_id: None,
                },
            );
            Some(result_reference)
        } else {
            None
        };
        self.entries.get_mut(reference).unwrap().completed =
            Some((remote.clone(), result_reference.clone()));
        describe(driver, session, remote, options, result_reference).await
    }
}

async fn release_group(driver: &DebuggerDriver, group: String) {
    if let Err(error) = driver.client().runtime().release_object_group(group).await {
        eprintln!("failed to release retained value group: {error:?}");
    }
}

async fn describe(
    driver: &mut DebuggerDriver,
    session: &SessionKey,
    remote: RuntimeRemoteObject,
    options: &DescribeOptions,
    reference: Option<String>,
) -> Result<ValueDescription, TargetDebuggerError> {
    let mut inspector = Inspector {
        driver,
        session,
        options,
        remaining: options.max_nodes,
        seen: Vec::new(),
        deadline: Instant::now() + Duration::from_secs(2),
    };
    let mut result = inspector.visit(remote, 0, "$".into()).await?;
    result.reference = reference;
    result.origin = Some("live".into());
    Ok(result)
}

struct Inspector<'a> {
    driver: &'a mut DebuggerDriver,
    session: &'a SessionKey,
    options: &'a DescribeOptions,
    remaining: u32,
    seen: Vec<(String, String)>,
    deadline: Instant,
}

impl Inspector<'_> {
    fn visit(
        &mut self,
        remote: RuntimeRemoteObject,
        depth: u32,
        path: String,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ValueDescription, TargetDebuggerError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            if self.remaining == 0 || Instant::now() >= self.deadline {
                let mut value = ValueDescription::new("limit");
                value.truncated = true;
                return Ok(value);
            }
            self.remaining -= 1;
            let kind = serialized_enum_name(&remote.r#type).unwrap_or_else(|| "unknown".into());
            let mut value = ValueDescription::new(&kind);
            value.summary = remote
                .description
                .clone()
                .or_else(|| remote.value.as_ref().map(ToString::to_string));
            if let Some(text) = value.summary.take() {
                let (text, truncated) = bounded_preview_text(&text, self.options.max_string_length);
                value.summary = Some(text);
                value.truncated |= truncated;
            }
            if remote.subtype == Some(RuntimeRemoteObjectSubtype::Null) {
                value.kind = "null".into();
                value.value = Some(Value::Null);
                return Ok(value);
            }
            if let Some(raw) = &remote.value {
                value.value = Some(if let Some(text) = raw.as_str() {
                    let (text, truncated) =
                        bounded_preview_text(text, self.options.max_string_length);
                    value.truncated |= truncated;
                    json!(text)
                } else {
                    raw.clone()
                });
                return Ok(value);
            }
            if let Some(raw) = &remote.unserializable_value {
                value.summary = Some(raw.clone());
                return Ok(value);
            }
            let Some(id) = remote.object_id.as_deref() else {
                return Ok(value);
            };
            for (seen, seen_path) in &self.seen {
                let same = if seen == id {
                    true
                } else {
                    let response = request(self.driver, "Runtime.callFunctionOn", json!({
                        "objectId": id, "functionDeclaration": "function(other){ return this === other; }",
                        "arguments":[{"objectId":seen}], "returnByValue":true,
                        "throwOnSideEffect":true, "silent":true,
                    })).await?;
                    response["result"]["value"] == true
                };
                if same {
                    value.kind = "reference".into();
                    value.identity = Some(seen_path.clone());
                    return Ok(value);
                }
            }
            self.seen.push((id.into(), path.clone()));
            let result = self.object(remote, value, depth, path).await;
            self.seen.pop();
            result
        })
    }

    async fn object(
        &mut self,
        remote: RuntimeRemoteObject,
        mut value: ValueDescription,
        depth: u32,
        path: String,
    ) -> Result<ValueDescription, TargetDebuggerError> {
        let id = remote.object_id.as_deref().unwrap();
        if depth >= self.options.max_depth {
            value.truncated = true;
            return Ok(value);
        }
        let (properties, internal) = descriptors(self.driver, id).await?;
        if depth == 0 {
            let mut source = crate::debugger::object_inspection::LiveSourceInspector::default();
            value.source = source
                .inspect(
                    self.driver,
                    self.session,
                    id,
                    Some((&properties, &internal)),
                )
                .await;
        }
        if remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise) {
            value.kind = "promise".into();
            let (state, result) = promise_parts(&internal);
            if state != "pending" {
                if let Some(result) = result {
                    value.result = Some(Box::new(
                        self.visit(result, depth + 1, format!("{path}.result"))
                            .await?,
                    ));
                }
            }
            value.state = Some(state);
            return Ok(value);
        }
        if value.kind == "function"
            || remote
                .subtype
                .as_ref()
                .is_some_and(|s| *s != RuntimeRemoteObjectSubtype::Array)
        {
            value.kind = remote
                .subtype
                .as_ref()
                .and_then(serialized_enum_name)
                .unwrap_or(value.kind);
            return Ok(value);
        }
        if remote
            .class_name
            .as_deref()
            .is_some_and(|c| c != "Object" && c != "Array")
        {
            value.kind = "instance".into();
        }
        let array = remote.subtype == Some(RuntimeRemoteObjectSubtype::Array);
        let start = if depth == 0 {
            self.options.start as u64
        } else {
            0
        };
        if array {
            value.kind = "array".into();
            let length = properties
                .iter()
                .find(|p| p.name == "length")
                .and_then(|p| p.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let end = length
                .min(start + self.options.max_properties as u64)
                .min(start + self.remaining as u64);
            value.truncated |= start > 0 || length > end;
            value.next_start = (length > end).then_some(end as u32);
            for index in start..end {
                if self.remaining == 0 {
                    value.truncated = true;
                    value.next_start = Some(index as u32);
                    break;
                }
                let name = index.to_string();
                let child = if let Some(property) = properties.iter().find(|p| p.name == name) {
                    self.property(property, depth + 1, format!("{path}[{index}]"))
                        .await?
                } else {
                    ValueDescription::new("hole")
                };
                value
                    .properties
                    .push(DescribedProperty { name, value: child });
            }
            if properties
                .iter()
                .any(|p| p.enumerable && p.name.parse::<u64>().map_or(true, |i| i >= length))
            {
                value.truncated = true;
            }
        } else {
            let enumerable = properties
                .iter()
                .filter(|p| p.enumerable)
                .collect::<Vec<_>>();
            let end = enumerable
                .len()
                .min(start as usize + self.options.max_properties as usize)
                .min(start as usize + self.remaining as usize);
            value.truncated |= start > 0 || enumerable.len() > end;
            value.next_start = (enumerable.len() > end).then_some(end as u32);
            for (index, property) in enumerable
                .into_iter()
                .enumerate()
                .take(end)
                .skip(start as usize)
            {
                if self.remaining == 0 {
                    value.truncated = true;
                    value.next_start = Some(index as u32);
                    break;
                }
                let child = self
                    .property(
                        property,
                        depth + 1,
                        format!("{path}[{}]", json!(property.name)),
                    )
                    .await?;
                value.properties.push(DescribedProperty {
                    name: property.name.clone(),
                    value: child,
                });
            }
        }
        Ok(value)
    }

    async fn property(
        &mut self,
        property: &RuntimePropertyDescriptor,
        depth: u32,
        path: String,
    ) -> Result<ValueDescription, TargetDebuggerError> {
        if property.symbol.is_some() {
            return Ok(ValueDescription::new("symbol-key"));
        }
        match &property.value {
            Some(remote) => self.visit(remote.clone(), depth, path).await,
            None => Ok(ValueDescription::new("accessor")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn await_detection_is_syntax_based_and_excludes_functions() {
        for source in [
            "'await foo()'",
            "/*await*/ 1",
            "(async () => await f(), 3)",
            "({async method() { await f(); }})",
        ] {
            assert!(!has_top_level_await(source).unwrap(), "{source}");
        }
        for source in ["await f()", "f(await g())", "({x: await g()})"] {
            assert!(has_top_level_await(source).unwrap(), "{source}");
        }
        assert!(has_top_level_await("await (").is_err());
    }
    #[test]
    fn references_are_opaque_and_kind_distinguished() {
        let a = new_reference(false).unwrap();
        assert!(a.starts_with("@v"));
        assert!(new_reference(true).unwrap().starts_with("@e"));
        assert_ne!(a, new_reference(false).unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires node on PATH; run explicitly for backend integration"]
    async fn live_owned_values_deadline_concurrency_and_cleanup() {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut child = tokio::process::Command::new("node")
            .args(["--inspect=0", "-e", "setInterval(()=>{},1000)"])
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let endpoint = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = lines.next_line().await.unwrap().unwrap();
                if let Some(endpoint) = line.strip_prefix("Debugger listening on ") {
                    break endpoint.to_owned();
                }
            }
        })
        .await
        .unwrap();
        let connection = crate::debugger::cdp_runtime::CdpConnection::connect_root_debugger(
            &endpoint,
            1,
            "owned".into(),
        )
        .await
        .unwrap();
        let session = connection.take_root_debugger_session().unwrap();
        let key = session.key().clone();
        let debugger = TargetDebuggerHandle::start(
            "owned".into(),
            "node".into(),
            "root".into(),
            1,
            session,
            key,
            false,
            Arc::new(ContextSourceModel::new()),
        )
        .await
        .unwrap();
        let eval = |expression: &str, retain: bool, await_result: bool| ValueOperation::Evaluate {
            expression: expression.into(),
            retain,
            await_result,
        };
        let options = DescribeOptions::default();
        let error = debugger
            .value_operation(
                eval("(() => { while (true) {} })()", false, false),
                options.clone(),
                50,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("timed out")
                || error.to_string().contains("Execution was terminated"),
            "{error}"
        );
        assert_eq!(
            debugger
                .value_operation(eval("42", false, false), options.clone(), 100)
                .await
                .unwrap()
                .json(true)
                .unwrap(),
            json!(42)
        );

        let pending = debugger
            .value_operation(
                eval("new Promise(r => globalThis.finish = r)", false, false),
                options.clone(),
                0,
            )
            .await
            .unwrap();
        let reference = pending.reference.unwrap();
        let waiting = debugger.value_operation(
            ValueOperation::Await {
                reference: reference.clone(),
            },
            options.clone(),
            5000,
        );
        let progress = async {
            tokio::time::sleep(Duration::from_millis(75)).await;
            debugger
                .value_operation(
                    eval("(finish({answer:42}), true)", false, false),
                    options.clone(),
                    100,
                )
                .await
                .unwrap()
        };
        let (settled, _) = tokio::join!(waiting, progress);
        assert_eq!(settled.unwrap().json(true).unwrap(), json!({"answer":42}));
        debugger
            .value_operation(
                ValueOperation::Release {
                    reference: reference.clone(),
                },
                options.clone(),
                0,
            )
            .await
            .unwrap();
        assert!(
            debugger
                .value_operation(ValueOperation::Show { reference }, options.clone(), 0)
                .await
                .is_err()
        );
        assert!(
            debugger
                .value_operation(
                    eval("'x'.repeat(10001)", false, false),
                    options.clone(),
                    100
                )
                .await
                .unwrap()
                .json(true)
                .is_err()
        );
        let shared = debugger
            .value_operation(
                eval("((v)=>({a:v,b:v}))({x:1})", false, false),
                options.clone(),
                100,
            )
            .await
            .unwrap();
        assert_eq!(shared.json(true).unwrap(), json!({"a":{"x":1},"b":{"x":1}}));

        let retained = debugger
            .value_operation(eval("({alive:true})", true, false), options.clone(), 0)
            .await
            .unwrap();
        // The same raw event stream drives target context-lifetime invalidation.
        debugger
            .raw_cdp_request(
                "Runtime.evaluate".into(),
                json!({"expression":"require('vm').runInNewContext('1')"}),
            )
            .await
            .unwrap();
        connection.close().await;
        assert!(
            debugger
                .value_operation(
                    ValueOperation::Show {
                        reference: retained.reference.unwrap()
                    },
                    options,
                    0
                )
                .await
                .is_err()
        );
        child.kill().await.unwrap();
    }
}
