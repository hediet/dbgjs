use std::env;
use std::sync::Arc;
use std::time::Duration;

use cdp_client::cdp::{
    InputDispatchKeyEventParams, InputDispatchKeyEventParamsType, InputInsertTextParams,
    RuntimeEvaluateParams, RuntimeRemoteObjectType, TargetAttachToTargetParams,
    TargetCloseTargetParams, TargetCreateTargetParams,
};
use cdp_client::cdp_runtime::CdpConnection;
use cdp_client::content_store::ContentStore;
use cdp_client::debugger_driver::{DebuggerDriver, DebuggerRecording};
use cdp_client::debugger_engine::{
    BreakpointBinding, BreakpointKey, DebuggerState, FrameProjection, Input, ScriptSourceState,
    SessionKey, SessionPhase,
};
use cdp_client::source_effects::{SourceEffectInterpreter, SourceEffectOptions};
use cdp_client::source_view::Position;
use serde_json::json;
use sourcemap::SourceMapBuilder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout};

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(30);
const VSCODE_SCENARIO_TIMEOUT: Duration = Duration::from_secs(180);
const GENERATED_URL: &str = "file:///playwright-cdp-e2e.js";
const AUTHORED_URL: &str = "file:///playwright-cdp-e2e.ts";
const AUTHORED_SOURCE: &str = "function add(a: number, b: number): number {\n  return a + b;\n}";
const VSCODE_CURSOR_SOURCE: &str = "../../../src/vs/editor/common/cursor/cursor.ts";
const VSCODE_WORKBENCH_SCRIPT_SUFFIX: &str = "/vs/workbench/workbench.web.main.internal.js";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launched by Playwright with CDP_WS_ENDPOINT"]
async fn generated_client_hits_a_real_breakpoint_in_playwright_chromium() {
    timeout(SCENARIO_TIMEOUT, run_breakpoint_scenario())
        .await
        .expect("live CDP breakpoint scenario timed out");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launched by Playwright with CDP_WS_ENDPOINT and VSCODE_TARGET_ID"]
async fn reducer_hits_an_authored_vscode_dev_typing_breakpoint() {
    timeout(VSCODE_SCENARIO_TIMEOUT, run_vscode_dev_scenario())
        .await
        .expect("vscode.dev debugger scenario timed out");
}

async fn run_breakpoint_scenario() {
    let (source_map_url, source_map_server) = serve_source_map().await;
    let endpoint = env::var("CDP_WS_ENDPOINT").expect("Playwright provides CDP_WS_ENDPOINT");
    let connection = CdpConnection::connect(&endpoint)
        .await
        .expect("connect to Playwright-launched Chromium CDP endpoint");
    let root = connection.root();

    let created = root
        .target_create_target(TargetCreateTargetParams::new("about:blank".into()))
        .await
        .expect("Target.createTarget failed");
    let mut attach_params = TargetAttachToTargetParams::new(created.target_id.clone());
    attach_params.flatten = Some(true);
    let attached = root
        .target_attach_to_target(attach_params)
        .await
        .expect("Target.attachToTarget failed");

    let session_key = SessionKey {
        connection_generation: 1,
        session_id: attached.session_id,
    };
    let session = connection
        .open_session(session_key.clone())
        .expect("child session opens");
    let sources = SourceEffectInterpreter::new(
        SourceEffectOptions::default(),
        Arc::new(ContentStore::default()),
    );
    let mut driver = DebuggerDriver::new(Arc::new(DebuggerState::default()), session, sources);
    driver
        .apply(Input::Connected)
        .await
        .expect("connection state initializes");
    driver
        .apply(Input::SessionAttached {
            session_id: session_key.session_id.clone(),
            target_id: created.target_id.clone(),
            parent_session_id: None,
            waiting_for_debugger: false,
        })
        .await
        .expect("session configures through reducer effects");

    let breakpoint_key = BreakpointKey {
        client_id: "playwright".into(),
        breakpoint_id: "add-return".into(),
    };
    driver
        .apply(Input::SetBreakpoint {
            key: breakpoint_key.clone(),
            source_url: AUTHORED_URL.into(),
            position: Position { line: 1, column: 2 },
            condition: None,
        })
        .await
        .expect("authored breakpoint intent is retained before its script exists");

    driver
        .client()
        .runtime_evaluate(RuntimeEvaluateParams::new(format!(
            "function add(a, b) {{\n  return a + b;\n}}\n//# sourceMappingURL={}\n//# sourceURL={GENERATED_URL}",
            source_map_url,
        )))
        .await
        .expect("function definition failed");

    while !matches!(
        driver.state().breakpoints[&breakpoint_key]
            .bindings
            .values()
            .next(),
        Some(BreakpointBinding::Installed { .. })
    ) {
        driver
            .process_next_event()
            .await
            .expect("script event processes through reducer");
    }
    assert!(driver.state().scripts.values().any(|script| {
        script.url == GENERATED_URL
            && matches!(
                &script.source,
                ScriptSourceState::Resolved(view)
                    if view.logical_sources.contains_key(AUTHORED_URL)
            )
    }));
    assert!(matches!(
        driver.state().breakpoints[&breakpoint_key]
            .bindings
            .values()
            .next(),
        Some(BreakpointBinding::Installed { .. })
    ));

    let evaluation_client = driver.client().clone();
    let evaluation = tokio::spawn(async move {
        let mut params = RuntimeEvaluateParams::new("add(20, 22)".into());
        params.return_by_value = Some(true);
        evaluation_client
            .runtime_evaluate(params)
            .await
            .expect("breakpoint evaluation failed")
    });

    while driver.state().sessions[&session_key].pause.is_none() {
        driver
            .process_next_event()
            .await
            .expect("pause event processes through reducer");
    }
    let pause = driver.state().sessions[&session_key]
        .pause
        .as_ref()
        .expect("session is paused");
    let top_frame = pause.frames.first().expect("pause has a call frame");
    assert_eq!(top_frame.function_name, "add");
    assert!(matches!(
        top_frame.projected,
        FrameProjection::Resolved {
            ref source_url,
            position: Position { line: 1, column: 2 },
        } if source_url == AUTHORED_URL
    ));

    driver
        .apply(Input::ResumeRequested {
            session: session_key.clone(),
            pause_epoch: pause.epoch,
        })
        .await
        .expect("resume command executes through reducer");
    while !matches!(
        driver.state().sessions[&session_key].phase,
        SessionPhase::Running
    ) {
        driver
            .process_next_event()
            .await
            .expect("resumed event processes through reducer");
    }

    let evaluated = evaluation.await.expect("evaluation task completes");
    assert_eq!(evaluated.result.r#type, RuntimeRemoteObjectType::Number);
    assert_eq!(evaluated.result.value, Some(json!(42)));

    driver
        .apply(Input::RemoveBreakpoint {
            key: breakpoint_key,
        })
        .await
        .expect("breakpoint removes through reducer effects");
    assert!(driver.state().breakpoints.is_empty());
    assert!(driver.state().physical_breakpoints.is_empty());

    let recording_json =
        serde_json::to_vec(driver.recording()).expect("debugger recording serializes");
    let recording: DebuggerRecording =
        serde_json::from_slice(&recording_json).expect("debugger recording deserializes");
    let replayed = recording
        .replay(Arc::new(DebuggerState::default()))
        .expect("debugger recording replays without effect divergence");
    assert!(!recording.transitions.is_empty());
    assert_eq!(replayed, *driver.state());
    source_map_server
        .await
        .expect("source-map server task completes");

    root.target_close_target(TargetCloseTargetParams::new(created.target_id))
        .await
        .expect("Target.closeTarget failed");
}

async fn run_vscode_dev_scenario() {
    let endpoint = env::var("CDP_WS_ENDPOINT").expect("Playwright provides CDP_WS_ENDPOINT");
    let target_id = env::var("VSCODE_TARGET_ID").expect("Playwright provides VSCODE_TARGET_ID");
    let connection = CdpConnection::connect(&endpoint)
        .await
        .expect("connect to Playwright-launched Chromium CDP endpoint");
    let root = connection.root();
    let mut attach_params = TargetAttachToTargetParams::new(target_id.clone());
    attach_params.flatten = Some(true);
    let attached = root
        .target_attach_to_target(attach_params)
        .await
        .expect("attach to vscode.dev target");
    let session_key = SessionKey {
        connection_generation: 1,
        session_id: attached.session_id,
    };
    let session = connection
        .open_session(session_key.clone())
        .expect("vscode.dev child session opens");
    let content_store = Arc::new(ContentStore::default());
    let sources =
        SourceEffectInterpreter::new(SourceEffectOptions::default(), content_store.clone());
    let mut driver = DebuggerDriver::new(Arc::new(DebuggerState::default()), session, sources);
    driver
        .apply(Input::Connected)
        .await
        .expect("connection state initializes");
    driver
        .apply(Input::SessionAttached {
            session_id: session_key.session_id.clone(),
            target_id,
            parent_session_id: None,
            waiting_for_debugger: false,
        })
        .await
        .expect("vscode.dev session configures");

    while !driver
        .state()
        .scripts
        .values()
        .any(|script| script.url.ends_with(VSCODE_WORKBENCH_SCRIPT_SUFFIX))
    {
        if let Err(error) = driver.process_next_event().await {
            let close_reason = connection.close_reason().lock().await.clone();
            let last_script = driver
                .state()
                .scripts
                .values()
                .last()
                .map(|script| script.url.clone());
            panic!(
                "vscode.dev script event failed: {error}; WebSocket close: {close_reason:?}; last script: {last_script:?}"
            );
        }
    }
    assert!(
        driver
            .state()
            .scripts
            .values()
            .all(|script| matches!(script.source, ScriptSourceState::Unresolved))
    );
    assert_eq!(content_store.stats().unique_utf8_bytes, 0);
    let workbench_script = driver
        .state()
        .scripts
        .iter()
        .find(|(_, script)| script.url.ends_with(VSCODE_WORKBENCH_SCRIPT_SUFFIX))
        .map(|(key, _)| key.clone())
        .expect("workbench script metadata is known");
    driver
        .apply(Input::RequestScriptSource {
            script: workbench_script,
        })
        .await
        .expect("explicit workbench source request resolves its source map");
    let cursor_content = driver
        .state()
        .scripts
        .values()
        .find_map(|script| match &script.source {
            ScriptSourceState::Resolved(view) => view.logical_sources.get(VSCODE_CURSOR_SOURCE),
            _ => None,
        })
        .and_then(|candidate| content_store.get(candidate.content))
        .expect("resolved Cursor source content is retained");
    let breakpoint_position = find_position(
        &cursor_content,
        "const reason = EditSources.cursor({ kind: 'type'",
    );

    dispatch_ctrl_n(driver.client()).await;
    sleep(Duration::from_secs(1)).await;

    let breakpoint_key = BreakpointKey {
        client_id: "playwright-vscode".into(),
        breakpoint_id: "cursor-type".into(),
    };
    driver
        .apply(Input::SetBreakpoint {
            key: breakpoint_key.clone(),
            source_url: VSCODE_CURSOR_SOURCE.into(),
            position: breakpoint_position,
            condition: None,
        })
        .await
        .expect("authored Cursor.type breakpoint installs");
    assert!(matches!(
        driver.state().breakpoints[&breakpoint_key]
            .bindings
            .values()
            .next(),
        Some(BreakpointBinding::Installed { .. })
    ));

    let input_client = driver.client().clone();
    let typing = tokio::spawn(async move {
        input_client
            .input_insert_text(InputInsertTextParams::new("vscode".into()))
            .await
            .expect("CDP text insertion succeeds")
    });
    while driver.state().sessions[&session_key].pause.is_none() {
        driver
            .process_next_event()
            .await
            .expect("vscode.dev pause event processes");
    }
    let pause = driver.state().sessions[&session_key]
        .pause
        .as_ref()
        .expect("vscode.dev session is paused");
    let top_frame = pause.frames.first().expect("vscode.dev pause has a frame");
    let FrameProjection::Resolved {
        source_url,
        position,
    } = &top_frame.projected
    else {
        panic!("top vscode.dev frame is not source-mapped");
    };
    assert_eq!(source_url, VSCODE_CURSOR_SOURCE);
    assert_eq!(position.line, breakpoint_position.line);

    driver
        .apply(Input::ResumeRequested {
            session: session_key.clone(),
            pause_epoch: pause.epoch,
        })
        .await
        .expect("vscode.dev resumes through reducer");
    while !matches!(
        driver.state().sessions[&session_key].phase,
        SessionPhase::Running
    ) {
        driver
            .process_next_event()
            .await
            .expect("vscode.dev resumed event processes");
    }
    typing.await.expect("typing task completes");

    driver
        .apply(Input::RemoveBreakpoint {
            key: breakpoint_key,
        })
        .await
        .expect("vscode.dev breakpoint removes");
    let replayed = driver
        .recording()
        .replay(Arc::new(DebuggerState::default()))
        .expect("vscode.dev reducer journal replays without Chrome");
    assert_eq!(replayed, *driver.state());
}

async fn dispatch_ctrl_n(
    client: &cdp_client::cdp::CdpClient<hubrpc::connection::channel::Channel>,
) {
    let mut key_down =
        InputDispatchKeyEventParams::new(InputDispatchKeyEventParamsType::RawKeyDown);
    key_down.modifiers = Some(2);
    key_down.code = Some("KeyN".into());
    key_down.key = Some("n".into());
    key_down.windows_virtual_key_code = Some(78);
    client
        .input_dispatch_key_event(key_down)
        .await
        .expect("Ctrl+N key-down succeeds");

    let mut key_up = InputDispatchKeyEventParams::new(InputDispatchKeyEventParamsType::KeyUp);
    key_up.modifiers = Some(2);
    key_up.code = Some("KeyN".into());
    key_up.key = Some("n".into());
    key_up.windows_virtual_key_code = Some(78);
    client
        .input_dispatch_key_event(key_up)
        .await
        .expect("Ctrl+N key-up succeeds");
}

fn find_position(source: &str, marker: &str) -> Position {
    let offset = source.find(marker).expect("authored source marker exists");
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    Position {
        line: u32::try_from(line).expect("source line fits u32"),
        column: u32::try_from(source[line_start..offset].encode_utf16().count())
            .expect("source column fits u32"),
    }
}

async fn serve_source_map() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("source-map server binds");
    let address = listener
        .local_addr()
        .expect("source-map server has address");
    let source_map = source_map_bytes();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("source-map request arrives");
        let mut request = [0; 4096];
        let request_length = socket
            .read(&mut request)
            .await
            .expect("source-map request is readable");
        assert!(
            String::from_utf8_lossy(&request[..request_length]).starts_with("GET /fixture.js.map ")
        );
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            source_map.len()
        );
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("source-map headers write");
        socket
            .write_all(&source_map)
            .await
            .expect("source-map body writes");
    });
    (format!("http://{address}/fixture.js.map"), task)
}

fn source_map_bytes() -> Vec<u8> {
    let mut builder = SourceMapBuilder::new(Some(GENERATED_URL));
    let source = builder.add_source(AUTHORED_URL);
    builder.set_source_contents(source, Some(AUTHORED_SOURCE));
    builder.add(0, 0, 0, 0, Some(AUTHORED_URL), None, false);
    builder.add(1, 2, 1, 2, Some(AUTHORED_URL), None, false);
    let mut bytes = Vec::new();
    builder
        .into_sourcemap()
        .to_writer(&mut bytes)
        .expect("source map serializes");
    bytes
}
