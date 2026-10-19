import UIKit
import WidgetKit

/// Usage over the relay: each host's agent logins with their live plan
/// windows (`ListAgentAccounts`) and its usage history (`GetUsageHistory`,
/// engines 0.5.1+). Feeds Settings → Usage and the Home / Lock Screen
/// widgets (through the App Group).
@MainActor
final class UsageFeed {
    private weak var app: AppModel?
    private(set) var accounts: [String: [AgentAccountView]] = [:]
    private(set) var accountErrors: [String: String] = [:]
    private var lastWidgetRead = Date.distantPast
    /// Don't re-probe providers for the widget more often than this.
    static let widgetInterval: TimeInterval = 5 * 60

    init(app: AppModel) {
        self.app = app
    }

    /// Hosts to ask, online first (the phone runs no agents).
    var hosts: [HostOption] {
        (app?.hostOptions ?? []).sorted { a, b in a.online != b.online ? a.online : a.name < b.name }
    }

    @discardableResult
    func loadAccounts(deviceId: String, force: Bool) async -> [AgentAccountView] {
        guard let client = app?.client else { return [] }
        do {
            let list = try await client.agentAccounts(deviceId: deviceId, force: force)
            accounts[deviceId] = list
            accountErrors[deviceId] = nil
            publishWidget(from: list)
            return list
        } catch {
            accountErrors[deviceId] = AppModel.describe(error)
            return accounts[deviceId] ?? []
        }
    }

    func history(deviceId: String, range: UsageRange) async throws -> UsageHistoryView {
        guard let client = app?.client else { throw CoreError.Closed }
        return try await client.usageHistory(deviceId: deviceId, range: range, timeZone: TimeZone.current.identifier)
    }

    /// Foreground / periodic: read the first online host's windows for the
    /// widgets (throttled; `force` re-probes the providers).
    func refreshWidget(force: Bool = false) async {
        guard let app, app.isSignedIn, !app.isDemo else { return }
        guard force || Date().timeIntervalSince(lastWidgetRead) > Self.widgetInterval,
              let host = hosts.first(where: \.online) else { return }
        lastWidgetRead = Date()
        await loadAccounts(deviceId: host.id, force: force)
    }

    private func publishWidget(from list: [AgentAccountView]) {
        guard let app, !app.isDemo else { return }
        guard let snapshot = Self.widgetSnapshot(list, now: Date()) else { return }
        if UsageWidgetStore.save(snapshot) {
            WidgetCenter.shared.reloadTimelines(ofKind: UsageWidgetStore.kind)
        }
    }

    /// The account the widgets follow: the active Claude Code login, else
    /// any active login, preferring one with a 5-hour window; its 5-hour
    /// window, else its most-used one (desktop `gauge_reading`).
    nonisolated static func widgetSnapshot(_ list: [AgentAccountView], now: Date) -> UsageWidgetSnapshot? {
        let active = list.filter { $0.active && !$0.windows.isEmpty }
        let hasFive = { (a: AgentAccountView) in a.windows.contains { $0.kind == .fiveHour } }
        guard let account = active.first(where: { $0.harness == "claude-code" && hasFive($0) })
            ?? active.first(where: hasFive)
            ?? active.first
        else { return nil }
        func window(_ w: UsageWindowView) -> UsageWidgetSnapshot.Window {
            let kind: UsageFormat.WindowKind = switch w.kind {
            case .fiveHour: .fiveHour
            case .weekly: .weekly
            case .other: .other
            }
            return .init(label: UsageFormat.windowLabel(w.label), kind: kind, used: w.usedFraction, resetsAtMs: w.resetsAtMs)
        }
        guard let main = account.windows.first(where: { $0.kind == .fiveHour })
            ?? account.windows.max(by: { $0.usedFraction < $1.usedFraction })
        else { return nil }
        let weekly = account.windows.first { $0.kind == .weekly }
        return UsageWidgetSnapshot(
            agent: account.harnessLabel,
            harness: account.harness,
            account: account.email ?? account.displayName ?? account.planLabel,
            window: window(main),
            weekly: weekly.map(window),
            fetchedAtMs: account.fetchedAtMs ?? Int64(now.timeIntervalSince1970 * 1000)
        )
    }
}
