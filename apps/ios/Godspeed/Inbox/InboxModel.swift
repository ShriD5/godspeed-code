import UIKit

/// What a row can do from the inbox.
enum InboxAction: Equatable {
    case approve
    case decline
    case dismiss
    case open
}

/// The "Needs you" queue as the inbox renders it: the core's merged queue
/// (every host, `CoreClient.attention()`), re-sorted defensively into the
/// desktop's order (risk band, then oldest first) and minus rows the user
/// just answered, so a swipe never bounces back while the relay round-trips.
///
/// Pure: built from FFI records, so the tests construct it directly. Push can
/// plug in later by mapping a notification's `itemId`/`deviceId` onto
/// `InboxModel.key` and routing to the inbox (see `AppModel.openInbox`).
struct InboxModel: Equatable {
    /// In display order.
    let items: [AttentionItemView]

    init(items: [AttentionItemView], hidden: Set<String> = []) {
        self.items = items
            .filter { !hidden.contains(Self.key($0)) }
            .sorted(by: Self.order)
    }

    /// Unique across hosts (two engines may mint the same item id).
    static func key(_ item: AttentionItemView) -> String {
        item.deviceId + "\u{0}" + item.id
    }

    static func order(_ a: AttentionItemView, _ b: AttentionItemView) -> Bool {
        if a.band != b.band { return a.band < b.band }
        if a.createdAtMs != b.createdAtMs { return a.createdAtMs < b.createdAtMs }
        if a.id != b.id { return a.id < b.id }
        return a.deviceId < b.deviceId
    }

    /// Everything waiting (the desktop sidebar's badge rule).
    var badgeCount: Int { items.count }

    var lowRisk: [AttentionItemView] { items.filter(\.batchApprovable) }

    var lowRiskCount: Int { lowRisk.count }

    func item(key: String) -> AttentionItemView? {
        items.first { Self.key($0) == key }
    }

    /// Tab / icon badge text: none at zero, "99+" past 99.
    static func badgeText(_ count: Int) -> String? {
        count <= 0 ? nil : count > 99 ? "99+" : String(count)
    }

    /// Swipe right approves, swipe left declines; rows that can't be
    /// answered here swipe left to dismiss (finished / failed) or not at all.
    static func swipes(_ item: AttentionItemView) -> (leading: InboxAction?, trailing: InboxAction?) {
        let leading: InboxAction? = item.canApprove ? .approve : nil
        let trailing: InboxAction? = item.canDecline ? .decline : item.canDismiss ? .dismiss : nil
        return (leading, trailing)
    }

    /// Tapping opens the chat; finished / failed / timed-out rows count as
    /// seen once opened (desktop `InboxPage::open`).
    static func dismissesOnOpen(_ item: AttentionItemView) -> Bool {
        item.canDismiss && !item.derived
    }

    // MARK: Presentation

    static func kindLabel(_ kind: AttentionKind) -> String {
        switch kind {
        case .approval: "Approval"
        case .question: "Question"
        case .timedOut: "Timed out"
        case .failed: "Failed"
        case .limited: "Limited"
        case .finished: "Finished"
        }
    }

    static func symbol(_ kind: AttentionKind) -> String {
        switch kind {
        case .approval: "checkmark.shield"
        case .question: "questionmark.bubble"
        case .timedOut: "clock.badge.exclamationmark"
        case .failed: "exclamationmark.triangle"
        case .limited: "hourglass"
        case .finished: "checkmark.circle"
        }
    }

    static func riskLabel(_ risk: ApprovalRisk) -> String {
        switch risk {
        case .low: "Low risk"
        case .medium: "Medium risk"
        case .high: "High risk"
        }
    }

    /// Desktop `risk_color`: success / warning / danger.
    static func riskColor(_ risk: ApprovalRisk?) -> UIColor {
        switch risk {
        case .low: Palette.success
        case .medium: Palette.warning
        case .high: Palette.danger
        case nil: Palette.accent
        }
    }

    static func tint(_ item: AttentionItemView) -> UIColor {
        switch item.kind {
        case .approval: riskColor(item.risk)
        case .question: StatusTone.input
        case .timedOut: Palette.warning
        case .failed: Palette.danger
        case .limited: StatusTone.limited
        case .finished: Palette.success
        }
    }

    /// The exact thing an approval runs: the command line, else the diff's
    /// first lines, else the paths it touches.
    static func detail(_ item: AttentionItemView) -> String? {
        guard let a = item.approval else { return nil }
        if let c = a.command, !c.isEmpty { return c }
        if let d = a.diff, !d.isEmpty {
            return d.split(separator: "\n", omittingEmptySubsequences: false).prefix(8).joined(separator: "\n")
        }
        if !a.paths.isEmpty { return a.paths.prefix(4).joined(separator: "\n") }
        return nil
    }

    /// "Project · Device · Claude Code".
    static func meta(_ item: AttentionItemView) -> String {
        var parts: [String] = []
        if let p = item.project, !p.isEmpty { parts.append(p) }
        if let d = item.deviceName, !d.isEmpty { parts.append(d) }
        if let agent = item.agentName { parts.append(agent) } else if let h = item.harness { parts.append(harnessLabel(harness: h)) }
        return parts.joined(separator: " · ")
    }

    /// One VoiceOver sentence for a row.
    static func accessibilityLabel(_ item: AttentionItemView, now: Date = Date()) -> String {
        var parts = [kindLabel(item.kind)]
        if let r = item.risk { parts.append(riskLabel(r)) }
        parts.append(item.title)
        if let a = item.approval {
            parts.append(a.title)
            if let c = a.command { parts.append("Command: \(c)") }
        } else if !item.summary.isEmpty {
            parts.append(item.summary)
        }
        let meta = meta(item)
        if !meta.isEmpty { parts.append(meta) }
        parts.append(UsageFormat.ago(ms: item.createdAtMs, now: now))
        return parts.joined(separator: ", ")
    }
}
