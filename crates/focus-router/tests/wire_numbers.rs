use focus_router::{ClaimReason, ErrorCode, InjectStatus, LeaveReason};

// The tracked verbatim copy of docs/protocol.proto keeps this test runnable in CI,
// where the ignored docs directory need not exist. No runtime filesystem access.
const PROTO: &str = include_str!("../../protocol/proto/protocol.proto");

fn wire_number(name: &str) -> u32 {
    PROTO
        .lines()
        .find_map(|line| {
            let (key, rest) = line.trim().split_once('=')?;
            (key.trim() == name).then(|| rest.split(';').next().unwrap().trim().parse().unwrap())
        })
        .unwrap_or_else(|| panic!("missing wire constant {name}"))
}

#[test]
fn enum_numbers_match_protocol_proto() {
    let entries = [
        (
            ErrorCode::UnsupportedSession.as_u32(),
            "ERROR_CODE_UNSUPPORTED_SESSION",
        ),
        (
            ErrorCode::CaptureFailed.as_u32(),
            "ERROR_CODE_CAPTURE_FAILED",
        ),
        (
            ErrorCode::TopologyChanged.as_u32(),
            "ERROR_CODE_TOPOLOGY_CHANGED",
        ),
        (ErrorCode::InvalidFrame.as_u32(), "ERROR_CODE_INVALID_FRAME"),
        (
            ErrorCode::InvalidSequence.as_u32(),
            "ERROR_CODE_INVALID_SEQUENCE",
        ),
        (
            ErrorCode::LeaseMismatch.as_u32(),
            "ERROR_CODE_LEASE_MISMATCH",
        ),
        (
            ErrorCode::InjectionFailed.as_u32(),
            "ERROR_CODE_INJECTION_FAILED",
        ),
        (ErrorCode::LeaseExpired.as_u32(), "ERROR_CODE_LEASE_EXPIRED"),
        (ErrorCode::AgentLost.as_u32(), "ERROR_CODE_AGENT_LOST"),
        (ErrorCode::NotOwner.as_u32(), "ERROR_CODE_NOT_OWNER"),
        (
            ErrorCode::StaleOwnerSerial.as_u32(),
            "ERROR_CODE_STALE_OWNER_SERIAL",
        ),
        (ErrorCode::CannotOwn.as_u32(), "ERROR_CODE_CANNOT_OWN"),
        (LeaveReason::ReturnEdge.as_u32(), "LEAVE_REASON_RETURN_EDGE"),
        (LeaveReason::Hotkey.as_u32(), "LEAVE_REASON_HOTKEY"),
        (LeaveReason::Emergency.as_u32(), "LEAVE_REASON_EMERGENCY"),
        (
            LeaveReason::ReceiverLocalInput.as_u32(),
            "LEAVE_REASON_RECEIVER_LOCAL_INPUT",
        ),
        (LeaveReason::AgentLost.as_u32(), "LEAVE_REASON_AGENT_LOST"),
        (
            LeaveReason::TopologyChanged.as_u32(),
            "LEAVE_REASON_TOPOLOGY_CHANGED",
        ),
        (
            LeaveReason::SuspendOrLock.as_u32(),
            "LEAVE_REASON_SUSPEND_OR_LOCK",
        ),
        (LeaveReason::Shutdown.as_u32(), "LEAVE_REASON_SHUTDOWN"),
        (
            LeaveReason::OwnershipLost.as_u32(),
            "LEAVE_REASON_OWNERSHIP_LOST",
        ),
        (ClaimReason::Startup.as_u32(), "CLAIM_REASON_STARTUP"),
        (ClaimReason::User.as_u32(), "CLAIM_REASON_USER"),
        (ClaimReason::Takeover.as_u32(), "CLAIM_REASON_TAKEOVER"),
        (InjectStatus::Ok.as_u32(), "INJECT_STATUS_OK"),
        (InjectStatus::Partial.as_u32(), "INJECT_STATUS_PARTIAL"),
        (InjectStatus::Failed.as_u32(), "INJECT_STATUS_FAILED"),
    ];
    for (actual, name) in entries {
        assert_eq!(actual, wire_number(name), "{name}");
    }
}
