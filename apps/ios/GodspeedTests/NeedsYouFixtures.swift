import Foundation
@testable import Godspeed

/// FFI records for the view-model tests (the core isn't needed).
enum Fixture {
    static let t0: Int64 = 1_790_000_000_000
    static let minute: Int64 = 60_000

    static func approval(
        _ id: String,
        device: String = "mac",
        risk: ApprovalRisk,
        minute m: Int64,
        chat: String? = nil,
        command: String? = "cargo test",
        diff: String? = nil,
        derived: Bool = false
    ) -> AttentionItemView {
        let band: UInt8 = switch risk {
        case .high: 0
        case .medium: 2
        case .low: 3
        }
        return AttentionItemView(
            id: id, deviceId: device, deviceName: device == "mac" ? "MacBook Pro" : device, kind: .approval,
            chatId: chat ?? "chat-\(id)", title: "Title \(id)", project: "godspeed", harness: "claude-code", agentName: nil,
            createdAtMs: t0 + m * minute, risk: risk, summary: "Run the tests", band: band,
            approval: ApprovalPreview(kindLabel: "Run command", title: "Run the tests", command: command, cwd: "/Users/dev/godspeed", paths: [], diff: diff, reason: nil, approveLabel: "Approve", declineLabel: "Decline"),
            canApprove: !derived, canDecline: !derived, canDismiss: false, batchApprovable: risk == .low && !derived, derived: derived
        )
    }

    static func item(_ id: String, kind: AttentionKind, device: String = "mac", minute m: Int64, canDismiss: Bool = false, derived: Bool = false) -> AttentionItemView {
        let band: UInt8 = switch kind {
        case .approval: 2
        case .question: 1
        case .failed: 4
        case .limited: 5
        case .timedOut: 6
        case .finished: 7
        }
        return AttentionItemView(
            id: id, deviceId: device, deviceName: "MacBook Pro", kind: kind, chatId: "chat-\(id)", title: "Title \(id)",
            project: "godspeed", harness: "codex", agentName: nil, createdAtMs: t0 + m * minute, risk: nil, summary: "Summary \(id)",
            band: band, approval: nil, canApprove: false, canDecline: false, canDismiss: canDismiss, batchApprovable: false, derived: derived
        )
    }

    static func row(
        _ id: String,
        device: String = "mac",
        deviceName: String = "MacBook Pro",
        project: String? = "space-godspeed",
        indicator: ChatIndicator,
        lastMinute: Int64 = 0,
        workingSinceMinute: Int64? = nil,
        branch: String? = nil,
        subagents: UInt32 = 0,
        backgroundWork: UInt32 = 0,
        limitResetMinute: Int64? = nil,
        lifecycle: Lifecycle = .active,
        archived: Bool = false,
        parent: String? = nil
    ) -> SessionRow {
        SessionRow(
            id: id, revision: 1, title: "Title \(id)", hasTitle: true, preview: "Preview \(id)",
            project: project.map { ProjectRef(id: $0, name: $0.replacingOccurrences(of: "space-", with: ""), colorIndex: 2) },
            deviceId: device, deviceName: deviceName, deviceOnline: true, harness: "claude-code", harnessLabel: "Claude Code",
            model: nil, modelLabel: nil, reasoning: nil, branch: branch, cwd: nil, indicator: indicator, hostIndicator: indicator,
            workingSinceMs: workingSinceMinute.map { t0 + $0 * minute }, lastActivityMs: t0 + lastMinute * minute, timeLabel: "now",
            createdAtMs: t0 - 3_600_000, unseen: indicator == .completed || indicator == .errored, archived: archived, pinned: false,
            sectionId: nil, pullRequest: nil, sendState: nil, parentChatId: parent, roomGen: 2, runningSubagents: subagents,
            backgroundWork: backgroundWork,
            limit: limitResetMinute.map { LimitInfo(limitedAtMs: t0 - 3_600_000, resetAtMs: t0 + $0 * minute, window: "5-hour", recovery: .ask) },
            lifecycle: lifecycle
        )
    }

    static func device(_ id: String, name: String, online: Bool = true) -> DeviceView {
        DeviceView(id: id, name: name, platform: "macos", online: online, lastSeenMs: nil, version: "0.5.2", capabilities: [], isExecutionHost: true, isSelf: false, sessionCount: 0)
    }

    static func project(_ id: String, name: String, device: String = "mac", group: String) -> ProjectView {
        ProjectView(id: id, name: name, path: "/src/\(name)", colorIndex: 3, deviceId: device, deviceName: nil, deviceOnline: true, gitDetected: true, createdAtMs: t0, groupKey: group, groupName: name, indicator: .idle, unseenCount: 0, sessions: [])
    }

    static func date(minute m: Int64) -> Date {
        Date(timeIntervalSince1970: Double(t0 + m * minute) / 1000)
    }
}
