import UIKit

/// A chat with sub-sessions (a conductor's sub-agents, side chats, chats an
/// agent created over MCP) as the phone shows it: who's in the family, how
/// each is doing, and what in it waits on you. The tree is the synced
/// `parentChatId` links; nothing here needs the conductor rows.
///
/// Pure: built from FFI records, so the tests construct it directly
/// (`docs/godspeed/MOBILE-MULTI-SESSION.md`).
struct FamilyMember: Hashable {
    /// Waiting on you first, then live work, then what's over.
    enum State: Int, Comparable {
        case needsYou, working, failed, done, idle

        static func < (a: State, b: State) -> Bool { a.rawValue < b.rawValue }

        var word: String {
            switch self {
            case .needsYou: "Needs you"
            case .working: "Working"
            case .failed: "Failed"
            case .done: "Done"
            case .idle: "Idle"
            }
        }

        var tint: UIColor {
            switch self {
            case .needsYou: StatusTone.input
            case .working: StatusTone.working
            case .failed: StatusTone.failed
            case .done: StatusTone.done
            case .idle: StatusTone.idle
            }
        }

        var glyph: StatusGlyph.Kind {
            switch self {
            case .working: .spinner
            case .done: .check(StatusTone.done)
            default: .dot(tint)
            }
        }
    }

    let id: String
    var title: String
    var state: State
    /// One line: what it's doing or waiting on.
    var activity: String
    /// Approvals and questions here or below.
    var waiting: Int
    /// Sessions below this one.
    var nested: Int
    /// Its host, when that isn't the family's host.
    var host: String?
    var hostOnline: Bool
    var deviceId: String
    var harness: String?
    var model: String?
    var preview: String?
    var createdAtMs: Int64
    var lastActivityMs: Int64
    var workingSinceMs: Int64?

    /// "Working · 4m", "Needs you · 2m", "Done · 3m ago".
    func statusLine(now: Date) -> String {
        switch state {
        case .working:
            guard let since = workingSinceMs else { return state.word }
            return "\(state.word) · \(UsageFormat.elapsed(fromMs: since, now: now))"
        case .needsYou:
            return "\(state.word) · \(UsageFormat.ago(ms: lastActivityMs, now: now))"
        case .failed, .done, .idle:
            return "\(state.word) · \(UsageFormat.ago(ms: lastActivityMs, now: now))"
        }
    }

    /// "on hetzner-01 · Sonnet 5 · ran 21m · 1 below".
    func details(now: Date) -> String {
        var parts: [String] = []
        if let host { parts.append("on \(host)\(hostOnline ? "" : " (offline)")") }
        if let model { parts.append(model) }
        let end = state == .working ? Int64(now.timeIntervalSince1970 * 1000) : lastActivityMs
        if end > createdAtMs { parts.append("ran \(UsageFormat.elapsed(fromMs: createdAtMs, now: Date(timeIntervalSince1970: Double(end) / 1000)))") }
        if nested > 0 { parts.append(nested == 1 ? "1 session below" : "\(nested) sessions below") }
        return parts.joined(separator: " · ")
    }
}

struct SessionFamily: Equatable {
    /// The page on screen.
    let currentId: String
    /// The chat whose sub-sessions are listed: the page itself when it has
    /// any, else its parent.
    let root: FamilyMember
    /// The root's own parent (the breadcrumb up), when the root is the page.
    let up: FamilyMember?
    /// The root's sub-sessions, waiting-on-you first.
    let members: [FamilyMember]
    /// What waits on you in the page and below it, inbox order.
    let waiting: [AttentionItemView]

    var rootIsCurrent: Bool { root.id == currentId }

    /// Strip entries in order (the breadcrumb, the root, its sub-sessions).
    enum Chip: Hashable {
        case up(FamilyMember)
        case root(FamilyMember, selected: Bool)
        case member(FamilyMember, selected: Bool)
        case all(Int)

        var id: String {
            switch self {
            case let .up(m): "up:\(m.id)"
            case let .root(m, _): "root:\(m.id)"
            case let .member(m, _): m.id
            case .all: "#all"
            }
        }
    }

    /// Past this many sub-sessions the strip ends with "All N" (a sheet).
    static let stripLimit = 4

    var chips: [Chip] {
        var out: [Chip] = []
        if rootIsCurrent, let up { out.append(.up(up)) }
        out.append(.root(root, selected: rootIsCurrent))
        out += members.map { .member($0, selected: $0.id == currentId) }
        if members.count > Self.stripLimit { out.append(.all(members.count)) }
        return out
    }

    /// Deepest nesting walked (conductors nest three levels; one spare).
    static let maxDepth = 4

    /// The family around `currentId`, or nil when it has no parent and no
    /// sub-sessions (a plain chat shows no strip).
    static func build(
        currentId: String,
        row: @escaping (String) -> SessionRow?,
        children: @escaping (String) -> [SessionRow],
        attention: [AttentionItemView]
    ) -> SessionFamily? {
        guard let current = row(currentId) else { return nil }
        func live(_ id: String) -> [SessionRow] { children(id).filter { !$0.archived && $0.id != id } }
        let rootRow: SessionRow
        var kids = live(current.id)
        if !kids.isEmpty {
            rootRow = current
        } else if let parentId = current.parentChatId, let parent = row(parentId) {
            rootRow = parent
            kids = live(parent.id)
        } else {
            return nil
        }

        // Every chat below a node, bounded and loop-safe.
        var below: [String: Set<String>] = [:]
        func descendants(_ id: String) -> Set<String> {
            if let hit = below[id] { return hit }
            var seen: Set<String> = [id]
            var frontier = [(id, 0)]
            var out = Set<String>()
            while let next = frontier.popLast() {
                let (node, depth) = next
                guard depth < maxDepth else { continue }
                for child in live(node) where seen.insert(child.id).inserted {
                    out.insert(child.id)
                    frontier.append((child.id, depth + 1))
                }
            }
            below[id] = out
            return out
        }

        let asks = attention.filter { $0.kind == .approval || $0.kind == .question }
        func waiting(in id: String) -> [AttentionItemView] {
            let scope = descendants(id).union([id])
            return asks.filter { item in item.chatId.map { scope.contains($0) } ?? false }.sorted(by: InboxModel.order)
        }

        func member(_ r: SessionRow) -> FamilyMember {
            let items = waiting(in: r.id)
            let own = items.first { $0.chatId == r.id } ?? items.first
            let state: FamilyMember.State
            if !items.isEmpty {
                state = .needsYou
            } else {
                switch r.indicator {
                case .awaitingInput, .awaitingApproval: state = .needsYou
                case .working: state = .working
                case .errored, .limited: state = .failed
                case .completed: state = r.backgroundWork > 0 ? .working : .done
                case .idle: state = r.backgroundWork > 0 ? .working : .idle
                }
            }
            return FamilyMember(
                id: r.id,
                title: r.title,
                state: state,
                activity: activity(r, state: state, asking: own),
                waiting: items.count,
                nested: descendants(r.id).count,
                host: r.deviceId == rootRow.deviceId ? nil : (r.deviceName ?? "another device"),
                hostOnline: r.deviceOnline,
                deviceId: r.deviceId,
                harness: r.harness,
                model: r.modelLabel ?? r.harnessLabel,
                preview: r.preview.map(clean).flatMap { $0.isEmpty ? nil : $0 },
                createdAtMs: r.createdAtMs,
                lastActivityMs: r.lastActivityMs,
                workingSinceMs: r.workingSinceMs
            )
        }

        let members = kids.map { member($0) }.sorted { a, b in
            if a.state != b.state { return a.state < b.state }
            if a.lastActivityMs != b.lastActivityMs { return a.lastActivityMs > b.lastActivityMs }
            return a.id < b.id
        }
        let up = rootRow.id == current.id ? current.parentChatId.flatMap { row($0) }.map { member($0) } : nil
        return SessionFamily(
            currentId: currentId,
            root: member(rootRow),
            up: up,
            members: members,
            waiting: waiting(in: current.id)
        )
    }

    /// What a sub-session shows under its title: the question or approval it
    /// waits on, else its latest message, else its state.
    static func activity(_ r: SessionRow, state: FamilyMember.State, asking: AttentionItemView?) -> String {
        if let asking {
            let mine = asking.chatId == r.id
            let what = asking.approval.map { "Approve: \($0.title)" } ?? (asking.summary.isEmpty ? "Waiting on your answer" : "Asks: \(clean(asking.summary))")
            return mine ? what : "Below: \(what)"
        }
        if state == .needsYou { return r.indicator == .awaitingApproval ? "Waiting on an approval" : "Waiting on your answer" }
        if let p = r.preview.map(clean), !p.isEmpty { return p }
        return state.word
    }

    /// One plain line from a preview: markdown marks and a live "Running…"
    /// tail dropped (desktop `clean_preview`).
    static func clean(_ preview: String) -> String {
        let plain = preview.filter { !"*`#".contains($0) }
        var line = plain.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        for tail in ["Running…", "Running..."] where line.hasSuffix(tail) { line.removeLast(tail.count) }
        return line.trimmingCharacters(in: .whitespaces)
    }

    // MARK: Rollups for lists

    /// Sub-session count and whether anything below waits on you, for a
    /// chat's row in the sessions list and on Mission Control.
    struct Rollup: Equatable {
        var children = 0
        var needsYou = false
    }

    static func rollup(
        _ id: String,
        children: (String) -> [SessionRow],
        attention: [AttentionItemView]
    ) -> Rollup {
        var r = Rollup()
        var seen: Set<String> = [id]
        var frontier = [(id, 0)]
        let asking = Set(attention.filter { $0.kind == .approval || $0.kind == .question }.compactMap(\.chatId))
        while let next = frontier.popLast() {
            let (node, depth) = next
            guard depth < maxDepth else { continue }
            for child in children(node) where !child.archived && seen.insert(child.id).inserted {
                if node == id { r.children += 1 }
                if child.indicator == .awaitingInput || child.indicator == .awaitingApproval || asking.contains(child.id) { r.needsYou = true }
                frontier.append((child.id, depth + 1))
            }
        }
        return r
    }
}
