import UIKit
import UserNotifications

/// The phone's "Needs you" inbox, thread lifecycle (settle / snooze / wake)
/// and usage-limit choices — the desktop's attention service and lifecycle,
/// driven over the relay and the synced registry.
extension AppModel {
    // MARK: Inbox

    /// Coalesce bursts of host snapshots into one rebuild per runloop turn.
    func scheduleAttentionRefresh() {
        guard !attentionRefreshScheduled else { return }
        attentionRefreshScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.attentionRefreshScheduled = false
            if self.refreshAttention(notify: false) { self.notifyObservers() }
        }
    }

    /// Pull the merged queue; true when what the inbox shows changed.
    @discardableResult
    func refreshAttention(notify: Bool = true) -> Bool {
        guard let client else { return false }
        let items = client.attention().items
        // Hidden rows the host has dropped need no hiding any more.
        let present = Set(items.map(InboxModel.key))
        inboxHidden.formIntersection(present)
        let next = InboxModel(items: items, hidden: inboxHidden)
        guard next != inbox else { return false }
        let countChanged = next.badgeCount != inbox.badgeCount
        inbox = next
        if countChanged { updateAppBadge(next.badgeCount) }
        if notify { notifyObservers() }
        return true
    }

    /// The app icon badge mirrors the inbox (only shows once the user has
    /// allowed notifications, which carry the badge permission).
    func updateAppBadge(_ count: Int) {
        guard !ProcessInfo.processInfo.arguments.contains("-demo") || count == 0 else { return }
        UNUserNotificationCenter.current().setBadgeCount(count) { _ in }
    }

    private func hide(_ item: AttentionItemView) {
        inboxHidden.insert(InboxModel.key(item))
        inbox = InboxModel(items: inbox.items, hidden: inboxHidden)
        updateAppBadge(inbox.badgeCount)
        notifyObservers()
    }

    /// The core keeps answered rows hidden until their host drops them (and
    /// brings failures back); this phone-side hide only covers the moment
    /// between the swipe and the call landing in the core.
    private func release(_ items: [AttentionItemView]) {
        inboxHidden.subtract(items.map(InboxModel.key))
        refreshAttention()
    }

    /// One-tap approve / decline. The row leaves at once; a failure brings it
    /// back and returns the reason.
    @MainActor
    func respond(_ item: AttentionItemView, approve: Bool) async -> String? {
        guard let client else { return "Not signed in" }
        hide(item)
        defer { release([item]) }
        do {
            try await client.respondAttention(deviceId: item.deviceId, itemId: item.id, approve: approve)
            return nil
        } catch {
            return Self.describe(error)
        }
    }

    /// Mark a finished / failed / timed-out row seen.
    @MainActor
    func dismiss(_ item: AttentionItemView) async {
        guard let client else { return }
        hide(item)
        defer { release([item]) }
        try? await client.dismissAttention(deviceId: item.deviceId, itemId: item.id)
    }

    @MainActor
    func approveAllLowRisk() async -> BatchResult {
        guard let client else { return BatchResult(approved: 0, skipped: 0, failedDevices: []) }
        let rows = inbox.lowRisk
        inboxHidden.formUnion(rows.map(InboxModel.key))
        inbox = InboxModel(items: inbox.items, hidden: inboxHidden)
        notifyObservers()
        let result = await client.approveLowRisk()
        release(rows)
        return result
    }

    /// Opening a row: finished / failed / timed-out ones count as seen.
    func opened(_ item: AttentionItemView) {
        if InboxModel.dismissesOnOpen(item) {
            Task { @MainActor in await self.dismiss(item) }
        }
    }

    static func describe(_ error: Error) -> String {
        switch error as? CoreError {
        case let .HostUnavailable(message): "\(message.components(separatedBy: ":").first ?? "The host") isn't reachable"
        case let .HostError(message): message
        case let .Unsupported(message): "Update Godspeed on the Mac (\(message))"
        case .NotFound: "Already answered"
        default: "Couldn't reach the host"
        }
    }

    // MARK: Thread lifecycle

    private var keyWindow: UIWindow? {
        UIApplication.shared.connectedScenes.compactMap { ($0 as? UIWindowScene)?.keyWindow }.first
    }

    private func writeLifecycle(_ id: String, _ lifecycle: Lifecycle) {
        do { try client?.setLifecycle(chatId: id, lifecycle: lifecycle) } catch { NSLog("lifecycle failed: \(error)") }
    }

    /// Done for now: onto the Settled shelf until new activity.
    func settle(_ id: String) {
        let before = row(id)?.lifecycle ?? .active
        writeLifecycle(id, .settled)
        Toast.show("Settled", action: "Undo", in: keyWindow) { [weak self] in self?.writeLifecycle(id, before) }
    }

    /// Out of the active list until `until` (or new activity).
    func snooze(_ id: String, until: Date) {
        let before = row(id)?.lifecycle ?? .active
        writeLifecycle(id, .snoozed(untilMs: Int64(until.timeIntervalSince1970 * 1000)))
        let label = UsageFormat.moment(ms: Int64(until.timeIntervalSince1970 * 1000), now: Date())
        Toast.show("Snoozed until \(label)", action: "Undo", in: keyWindow) { [weak self] in self?.writeLifecycle(id, before) }
    }

    /// Back to the active list.
    func wake(_ id: String) {
        writeLifecycle(id, .active)
    }

    /// The desktop's snooze presets, from `now`.
    static func snoozePresets(now: Date = Date(), calendar: Calendar = .current) -> [(title: String, date: Date)] {
        var out: [(String, Date)] = [("1 Hour", now.addingTimeInterval(3_600)), ("3 Hours", now.addingTimeInterval(3 * 3_600))]
        let today = calendar.startOfDay(for: now)
        if let evening = calendar.date(byAdding: .hour, value: 18, to: today), evening > now.addingTimeInterval(3_600) {
            out.append(("This Evening", evening))
        }
        if let tomorrow = calendar.date(byAdding: .day, value: 1, to: today),
           let morning = calendar.date(byAdding: .hour, value: 9, to: tomorrow) {
            out.append(("Tomorrow Morning", morning))
        }
        if let monday = calendar.nextDate(after: now, matching: DateComponents(hour: 9, weekday: 2), matchingPolicy: .nextTime) {
            out.append(("Next Week", monday))
        }
        return out
    }

    /// Settle / Snooze / Wake as a menu for a row.
    func lifecycleMenu(for id: String) -> UIMenu {
        let lifecycle = row(id)?.lifecycle ?? .active
        if lifecycle != .active {
            return UIMenu(options: .displayInline, children: [
                UIAction(title: "Wake", image: UIImage(systemName: "sunrise")) { [weak self] _ in self?.wake(id) },
            ])
        }
        let snooze = UIMenu(title: "Snooze", image: UIImage(systemName: "moon.zzz"), children: Self.snoozePresets().map { preset in
            UIAction(title: preset.title, subtitle: UsageFormat.moment(ms: Int64(preset.date.timeIntervalSince1970 * 1000), now: Date())) { [weak self] _ in
                self?.snooze(id, until: preset.date)
            }
        })
        return UIMenu(options: .displayInline, children: [
            UIAction(title: "Settle", image: UIImage(systemName: "checkmark.circle")) { [weak self] _ in self?.settle(id) },
            snooze,
        ])
    }

    // MARK: Usage limits

    /// A Limited chat's choice, sent to its host (which owns the timer).
    @MainActor
    func limitAction(_ chatId: String, _ action: LimitAction) async -> String? {
        guard let client else { return "Not signed in" }
        do {
            try await client.setLimitRecovery(chatId: chatId, action: action, resetAtMs: nil)
            return nil
        } catch {
            return Self.describe(error)
        }
    }
}
