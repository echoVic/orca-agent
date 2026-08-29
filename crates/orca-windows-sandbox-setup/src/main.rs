#![deny(deprecated)]

use std::io::{self, BufRead, Write as _};
use std::path::PathBuf;

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct SetupRequest {
    version: u32,
    operation: SetupOperation,
    state_dir: PathBuf,
    workspace: PathBuf,
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum SetupOperation {
    Provision,
    Check,
    Repair,
    Remove,
}

#[derive(Debug, serde::Serialize)]
struct SetupResponse {
    version: u32,
    ok: bool,
    receipt: Option<SetupReceipt>,
    removed: bool,
    error: Option<String>,
}

type SetupReceipt = orca_windows_sandbox::SandboxSetupReceipt;

fn main() {
    let result = run();
    if let Err(error) = result {
        let _ = write_response(SetupResponse {
            version: PROTOCOL_VERSION,
            ok: false,
            receipt: None,
            removed: false,
            error: Some(error.to_string()),
        });
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let frame = read_bounded_frame(&mut reader)?;
    let request: SetupRequest = serde_json::from_slice(frame.trim_ascii())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let response = handle(request);
    write_response(response)
}

fn read_bounded_frame(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let consumed = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |position| position + 1);
        if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sandbox setup request exceeds the frame limit",
            ));
        }
        let has_newline = chunk[..consumed].contains(&b'\n');
        frame.extend_from_slice(&chunk[..consumed]);
        reader.consume(consumed);
        if has_newline {
            break;
        }
    }
    if frame.is_empty() || !frame.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "sandbox setup request must be newline-terminated",
        ));
    }
    Ok(frame)
}

fn write_response(response: SetupResponse) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(windows)]
fn handle(request: SetupRequest) -> SetupResponse {
    use orca_windows_sandbox::CapabilityStore;

    let invalid = |error: String| SetupResponse {
        version: PROTOCOL_VERSION,
        ok: false,
        receipt: None,
        removed: false,
        error: Some(error),
    };
    if request.version != PROTOCOL_VERSION {
        return invalid(format!(
            "unsupported sandbox setup protocol version {}",
            request.version
        ));
    }
    if !request.workspace.is_absolute() || !request.state_dir.is_absolute() {
        return invalid("state_dir and workspace must be absolute Windows paths".to_string());
    }
    let store = CapabilityStore::new(&request.state_dir);
    let operation = request.operation;
    if matches!(operation, SetupOperation::Remove) {
        return match store.remove_setup(
            &request.workspace,
            orca_windows_sandbox::SETUP_HELPER_VERSION,
        ) {
            Ok(removed) => SetupResponse {
                version: PROTOCOL_VERSION,
                ok: true,
                receipt: None,
                removed,
                error: None,
            },
            Err(error) => invalid(error.to_string()),
        };
    }
    let receipt = match operation {
        SetupOperation::Provision | SetupOperation::Repair => {
            if let Err(error) = orca_windows_sandbox::ensure_appcontainer_profile() {
                return invalid(format!(
                    "failed to provision Windows AppContainer profile: {error}"
                ));
            }
            let result = match operation {
                SetupOperation::Provision => store.provision_setup(
                    &request.workspace,
                    orca_windows_sandbox::SETUP_HELPER_VERSION,
                ),
                SetupOperation::Repair => store.repair_setup(
                    &request.workspace,
                    orca_windows_sandbox::SETUP_HELPER_VERSION,
                ),
                SetupOperation::Check | SetupOperation::Remove => unreachable!(),
            };
            match result {
                Ok(receipt) => receipt,
                Err(error) => return invalid(error.to_string()),
            }
        }
        SetupOperation::Check => {
            match store.verify_setup_for_workspace(
                &request.workspace,
                orca_windows_sandbox::SETUP_HELPER_VERSION,
            ) {
                Ok(receipt) => receipt,
                Err(error) => return invalid(error.to_string()),
            }
        }
        SetupOperation::Remove => unreachable!(),
    };
    SetupResponse {
        version: PROTOCOL_VERSION,
        ok: true,
        receipt: Some(receipt),
        removed: false,
        error: None,
    }
}

#[cfg(not(windows))]
fn handle(_request: SetupRequest) -> SetupResponse {
    SetupResponse {
        version: PROTOCOL_VERSION,
        ok: false,
        receipt: None,
        removed: false,
        error: Some("orca-windows-sandbox-setup is only available on Windows".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_frame_requires_newline() {
        let mut reader = Cursor::new(br#"{"version":1}"#.to_vec());
        let error = read_bounded_frame(&mut reader).expect_err("unterminated frame");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bounded_frame_rejects_oversized_input() {
        let mut reader = Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]);
        let error = read_bounded_frame(&mut reader).expect_err("oversized frame");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn setup_request_rejects_unknown_fields() {
        let frame = br#"{"version":1,"operation":"check","state_dir":"C:\\orca","workspace":"C:\\work","extra":true}"#;
        let error = serde_json::from_slice::<SetupRequest>(frame).expect_err("unknown field");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn setup_request_accepts_repair_and_remove_operations() {
        for operation in ["repair", "remove"] {
            let frame = format!(
                r#"{{"version":1,"operation":"{operation}","state_dir":"C:\\orca","workspace":"C:\\work"}}"#
            );
            serde_json::from_str::<SetupRequest>(&frame)
                .unwrap_or_else(|error| panic!("operation {operation} should parse: {error}"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_setup_provisions_and_checks_profile_receipt() {
        let root = tempfile::tempdir().expect("setup tempdir");
        let state_dir = root.path().join("capabilities");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let provisioned = handle(SetupRequest {
            version: PROTOCOL_VERSION,
            operation: SetupOperation::Provision,
            state_dir: state_dir.clone(),
            workspace: workspace.clone(),
        });
        assert!(provisioned.ok, "setup provision response: {provisioned:?}");
        assert!(provisioned.receipt.is_some());
        let checked = handle(SetupRequest {
            version: PROTOCOL_VERSION,
            operation: SetupOperation::Check,
            state_dir,
            workspace,
        });
        assert!(checked.ok, "setup check response: {checked:?}");
        assert!(checked.receipt.is_some());
        let repaired = handle(SetupRequest {
            version: PROTOCOL_VERSION,
            operation: SetupOperation::Repair,
            state_dir: root.path().join("capabilities"),
            workspace: root.path().join("workspace"),
        });
        assert!(repaired.ok, "setup repair response: {repaired:?}");
        let removed = handle(SetupRequest {
            version: PROTOCOL_VERSION,
            operation: SetupOperation::Remove,
            state_dir: root.path().join("capabilities"),
            workspace: root.path().join("workspace"),
        });
        assert!(removed.ok, "setup remove response: {removed:?}");
        assert!(removed.removed);
        assert!(removed.receipt.is_none());
    }
}
