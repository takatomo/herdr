//! `core.terminal.run` on the API connection's thread: the core may take a
//! while to start a template's agent, and the app loop must not wait for it.

use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::api::schema::{
    CoreRunFirstMessage, CoreRunTerminal, CoreStatusParams, CoreTerminalOpenParams,
    CoreTerminalRunParams, ErrorBody, ErrorResponse, Method, Request, ResponseResult,
    SuccessResponse,
};
use crate::api::server::{dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT};
use crate::api::ApiRequestSender;
use crate::core_client::rpc::{call_once, CallError, CallOutcome};

const RUN_TIMEOUT: Duration = Duration::from_secs(180);

fn error(id: String, code: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    })
    .unwrap_or_default()
}

/// The environment a core terminal started from Herdr gets on top of the
/// core's own: only Herdr's terminal identity, never `HERDR_*`.
pub(crate) fn run_env() -> Map<String, Value> {
    crate::pane::pane_terminal_identity_env()
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect()
}

pub(crate) fn run_params(params: &CoreTerminalRunParams) -> Value {
    let mut value = json!({
        "argv": params.argv,
        "env": run_env(),
        "labels": params.labels,
        "outlive_parent": params.outlive_parent,
        "keep": params.keep,
        "no_notify": params.no_notify,
    });
    for (key, field) in [
        ("line", &params.line),
        ("cwd", &params.cwd),
        ("name", &params.name),
        ("parent", &params.parent),
        ("on", &params.on),
    ] {
        if let Some(field) = field {
            value[key] = Value::String(field.clone());
        }
    }
    value
}

/// The core's word on a template run's first message, passed on as is.
pub(crate) fn first_message(record: &Value) -> Option<CoreRunFirstMessage> {
    serde_json::from_value(record.get("first_message")?.clone()).ok()
}

pub(super) fn run_core_terminal(
    request_id: String,
    params: CoreTerminalRunParams,
    api_tx: &ApiRequestSender,
) -> String {
    let status = dispatch_to_app_with_timeout(
        Request {
            id: format!("{request_id}:status"),
            method: Method::CoreStatus(CoreStatusParams {}),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let status = match serde_json::from_str::<SuccessResponse>(&status) {
        Ok(SuccessResponse {
            result: ResponseResult::CoreStatus { status },
            ..
        }) => status,
        _ => {
            return error(
                request_id,
                "core_unavailable",
                "could not read the core status",
            )
        }
    };
    let (Some(endpoint), Some(core)) = (status.endpoint.clone(), status.core.clone()) else {
        return error(
            request_id,
            "core_unavailable",
            format!("herdr-core is {}", status.state),
        );
    };
    if status.state != "connected" {
        return error(
            request_id,
            "core_unavailable",
            format!("herdr-core is {}", status.state),
        );
    }
    if let Some(expected) = params.endpoint_core_id.as_deref() {
        if expected != core.id {
            return error(
                request_id,
                "core_mismatch",
                format!("Herdr is connected to core {}, not {expected}", core.id),
            );
        }
    }
    for (what, value) in [("parent", &params.parent), ("on", &params.on)] {
        let Some(value) = value else { continue };
        let machine = if what == "on" {
            Some(value.as_str())
        } else {
            crate::core_client::model::split_path(value).0
        };
        if let Some(machine) = machine {
            if !status.machines.iter().any(|known| known.name == machine) {
                return error(
                    request_id,
                    "core_mismatch",
                    format!("{what} {value} is not on this core or its machines"),
                );
            }
        }
    }
    let outcome = call_once(
        std::path::Path::new(&endpoint),
        &core.id,
        "terminal.run",
        run_params(&params),
        RUN_TIMEOUT,
    );
    let record = match outcome {
        Err(CallError::Unreachable(message)) => {
            return error(request_id, "core_unavailable", message)
        }
        Err(CallError::Mismatch { expected, found }) => {
            return error(
                request_id,
                "core_mismatch",
                format!("the socket now answers as core {found}, not {expected}; nothing was run"),
            )
        }
        Ok(CallOutcome::Refused(refused)) => {
            let code = refused.kind.unwrap_or_else(|| "core_refused".into());
            return error(request_id, &code, refused.message);
        }
        Ok(CallOutcome::Unknown(message)) => {
            let hint = params
                .name
                .as_deref()
                .map(|name| format!("; look for the name {name} in `herdr core list`"))
                .unwrap_or_default();
            return error(
                request_id,
                "run_result_unknown",
                format!("the core may have started it ({message}); do not run it again{hint}"),
            );
        }
        Ok(CallOutcome::Done(record)) => record,
    };
    let path = record
        .get("address")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let terminal = CoreRunTerminal {
        path: path.clone(),
        name: record
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        endpoint_core_id: core.id.clone(),
    };
    let first_message = first_message(&record);
    let Some(open) = params.open else {
        return serde_json::to_string(&SuccessResponse {
            id: request_id,
            result: ResponseResult::CoreTerminalRun {
                terminal,
                first_message,
                opened: None,
                open_error: None,
            },
        })
        .unwrap_or_default();
    };
    let opened = dispatch_to_app_with_timeout(
        Request {
            id: format!("{request_id}:open"),
            method: Method::CoreTerminalOpen(CoreTerminalOpenParams {
                endpoint_core_id: Some(core.id),
                terminal: path,
                mode: open.mode,
                placement: open.placement,
                target_pane_id: None,
                direction: None,
                focus: open.focus,
                close_popup: open.close_popup,
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let (opened, open_error) = match serde_json::from_str::<SuccessResponse>(&opened) {
        Ok(SuccessResponse {
            result: ResponseResult::CoreTerminalOpened { opened },
            ..
        }) => (Some(opened), None),
        _ => (
            None,
            Some(
                serde_json::from_str::<ErrorResponse>(&opened)
                    .map(|response| response.error)
                    .unwrap_or(ErrorBody {
                        code: "open_failed".into(),
                        message: "the terminal started but could not be opened".into(),
                    }),
            ),
        ),
    };
    serde_json::to_string(&SuccessResponse {
        id: request_id,
        result: ResponseResult::CoreTerminalRun {
            terminal,
            first_message,
            opened,
            open_error,
        },
    })
    .unwrap_or_default()
}
