import UIKit

/// Mission Control on the phone: every agent across your devices, grouped by
/// device then project, built exactly like the desktop board
/// (`crates/ui/src/mission_control/board.rs`). Pure: built from FFI rows so
/// the tests construct it directly.
struct BoardModel: Equatable {
    /// Most urgent first (the in-project sort order).
    enum Status: Int, Comparable {
        case needsYou, failed, limited, working, finished

        static func < (a: Status, b: Status) -> Bool { a.rawValue < b.rawValue }

        var label: String {
            switch self {
            case .needsYou: "Needs you"
            case .failed: "Failed"
            case .limited: "Limited"
            case .working: "Working"
            case .finished: "Finished"
            }
        }

        /// On the board whatever its age.
        var live: Bool { self == .needsYou || self == .limited || self == .working }

        /// The sidebar's display status folded into five; a settled turn
        /// still waiting on background work reads as working.
        init(_ indicator: ChatIndicator, backgroundWork: UInt32) {
            switch indicator {
            case .working: self = .working
            case .awaitingInput, .awaitingApproval: self = .needsYou
            case .limited: self = .limited
            case .errored: self = .failed
            case .completed, .idle: self = backgroundWork > 0 ? .working : .finished
            }
        }

        var tint: UIColor {
            switch self {
            case .needsYou: Palette.warning
            case .failed: Palette.danger
            case .limited: StatusTone.limited
            case .working: Palette.accent
            case .finished: Palette.success
            }
        }
    }

    struct Card: Hashable {
        let id: String
        var title: String
        var harness: String?
        var status: Status
        /// "Working · 4m", "Needs you · 2m ago", "Limited · back in 47m".
        var statusLabel: String
        /// What it is doing or waiting on (one line).
        var activity: String?
        var branch: String?
        var subagents: UInt32
        /// Finished or failed and not seen yet.
        var fresh: Bool
        var lastActivityMs: Int64
        /// The live turn's start, for the ticking timer.
        var workingSinceMs: Int64?
        /// A low-risk approval waiting on it (the one-tap approve).
        var approvableKey: String?
    }

    struct ProjectGroup: Hashable {
        let key: String
        var name: String
        var colorIndex: Int
        var hasProject: Bool
        var cards: [Card]
    }

    struct DeviceGroup: Hashable {
        let id: String
        var name: String
        var online: Bool
        var projects: [ProjectGroup]

        var count: Int { projects.reduce(0) { $0 + $1.cards.count } }
    }

    struct Totals: Equatable {
        var working = 0
        var needsYou = 0
        var limited = 0
        var finishedToday = 0
    }

    /// Failed and finished chats stay this long after their last message.
    static let recentHours: Double = 24

    var devices: [DeviceGroup] = []
    var totals = Totals()

    var isEmpty: Bool { devices.isEmpty }

    static func build(
        rows: [SessionRow],
        projects: [ProjectView],
        devices: [DeviceView],
        attention: [AttentionItemView],
        now: Date,
        calendar: Calendar = .current
    ) -> BoardModel {
        let nowMs = Int64(now.timeIntervalSince1970 * 1000)
        let recentSince = nowMs - Int64(recentHours * 3_600_000)
        let todayStart = Int64(calendar.startOfDay(for: now).timeIntervalSince1970 * 1000)
        let projectsById = Dictionary(projects.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var asking: [String: AttentionItemView] = [:]
        var approvable: [String: AttentionItemView] = [:]
        for item in attention {
            guard let chat = item.chatId else { continue }
            if (item.kind == .approval || item.kind == .question), asking[chat] == nil { asking[chat] = item }
            if item.batchApprovable, approvable[chat] == nil { approvable[chat] = item }
        }

        var totals = Totals()
        var order: [String] = []
        var buckets: [String: [ProjectGroup]] = [:]
        var seen = Set<String>()
        for row in rows where !row.archived && row.parentChatId == nil && seen.insert(row.id).inserted {
            let status = Status(row.indicator, backgroundWork: row.backgroundWork)
            let putAway = row.lifecycle != .active
            if !status.live && (putAway || row.lastActivityMs < recentSince) { continue }
            let ask = asking[row.id]
            let card = Card(
                id: row.id,
                title: row.title,
                harness: row.harness,
                status: status,
                statusLabel: statusLabel(row, status: status, asking: ask, nowMs: nowMs),
                activity: activity(row, status: status, asking: ask),
                branch: row.branch.map { $0.trimmingCharacters(in: .whitespaces) }.flatMap { $0.isEmpty ? nil : $0 },
                subagents: row.runningSubagents,
                fresh: row.indicator == .completed || row.indicator == .errored,
                lastActivityMs: row.lastActivityMs,
                workingSinceMs: status == .working && (row.indicator == .working) ? row.workingSinceMs : nil,
                approvableKey: approvable[row.id].map(InboxModel.key)
            )
            switch status {
            case .working: totals.working += 1
            case .needsYou: totals.needsYou += 1
            case .limited: totals.limited += 1
            case .finished where row.lastActivityMs >= todayStart: totals.finishedToday += 1
            default: break
            }
            let key: String
            let name: String
            let color: Int
            if let ref = row.project {
                let project = projectsById[ref.id]
                key = project?.groupKey ?? ref.id
                name = project?.groupName ?? ref.name
                color = Int(project?.colorIndex ?? ref.colorIndex)
            } else {
                key = "home"
                name = "No project"
                color = Int(projectColorIndex(spacePath: "home"))
            }
            if buckets[row.deviceId] == nil {
                order.append(row.deviceId)
                buckets[row.deviceId] = []
            }
            if let i = buckets[row.deviceId]!.firstIndex(where: { $0.key == key }) {
                buckets[row.deviceId]![i].cards.append(card)
            } else {
                buckets[row.deviceId]!.append(ProjectGroup(key: key, name: name, colorIndex: color, hasProject: row.project != nil, cards: [card]))
            }
        }

        let deviceById = Dictionary(devices.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var groups: [DeviceGroup] = order.map { id in
            var projects = buckets[id] ?? []
            for i in projects.indices {
                projects[i].cards.sort { a, b in
                    if a.status != b.status { return a.status < b.status }
                    if a.lastActivityMs != b.lastActivityMs { return a.lastActivityMs > b.lastActivityMs }
                    return a.id < b.id
                }
            }
            projects.sort { a, b in
                let la = a.cards[0].status, lb = b.cards[0].status
                if la != lb { return la < lb }
                let ta = a.cards.map(\.lastActivityMs).max() ?? 0, tb = b.cards.map(\.lastActivityMs).max() ?? 0
                if ta != tb { return ta > tb }
                if a.name.lowercased() != b.name.lowercased() { return a.name.lowercased() < b.name.lowercased() }
                return a.key < b.key
            }
            let device = deviceById[id]
            let rowName = rows.first { $0.deviceId == id }?.deviceName
            return DeviceGroup(id: id, name: device?.name ?? rowName ?? "Unknown device", online: device?.online ?? false, projects: projects)
        }
        // Online hosts first (the phone hosts nothing), then by name.
        groups.sort { a, b in
            if a.online != b.online { return a.online }
            if a.name.lowercased() != b.name.lowercased() { return a.name.lowercased() < b.name.lowercased() }
            return a.id < b.id
        }
        return BoardModel(devices: groups, totals: totals)
    }

    /// `<1m`, `4m`, `1h 5m` (desktop `working_duration_label`).
    static func durationLabel(ms: Int64) -> String {
        let minutes = max(0, ms / 60_000)
        switch (minutes / 60, minutes % 60) {
        case (0, 0): return "<1m"
        case let (0, m): return "\(m)m"
        case let (h, 0): return "\(h)h"
        case let (h, m): return "\(h)h \(m)m"
        }
    }

    static func statusLabel(_ row: SessionRow, status: Status, asking: AttentionItemView?, nowMs: Int64) -> String {
        func ago(_ ms: Int64) -> String {
            let text = UsageFormat.ago(ms: ms, now: Date(timeIntervalSince1970: Double(nowMs) / 1000))
            return text == "now" ? "just now" : text
        }
        switch status {
        case .working where row.indicator == .completed || row.indicator == .idle:
            let n = row.backgroundWork
            return "Waiting on \(n) task\(n == 1 ? "" : "s")"
        case .working:
            guard let since = row.workingSinceMs else { return "Working" }
            return "Working · " + durationLabel(ms: nowMs - since)
        case .needsYou:
            return "Needs you · " + ago(asking?.createdAtMs ?? row.lastActivityMs)
        case .limited:
            if let reset = row.limit?.resetAtMs, reset > nowMs {
                return "Limited · back in " + durationLabel(ms: reset - nowMs)
            }
            return "Limited"
        case .failed:
            return "Failed · " + ago(row.lastActivityMs)
        case .finished:
            return "Finished · " + ago(row.lastActivityMs)
        }
    }

    static func activity(_ row: SessionRow, status: Status, asking: AttentionItemView?) -> String? {
        var text: String?
        if status == .needsYou, let item = asking {
            text = item.approval?.title ?? (item.summary.isEmpty ? item.title : item.summary)
        }
        text = text ?? row.preview
        let line = text?.split(whereSeparator: \.isNewline).joined(separator: " ").trimmingCharacters(in: .whitespaces)
        return line?.isEmpty == false ? line : nil
    }
}
