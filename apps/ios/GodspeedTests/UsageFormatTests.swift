import XCTest
@testable import Godspeed

final class UsageFormatTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_790_000_000)
    private func ms(_ seconds: TimeInterval) -> Int64 { Int64((now.timeIntervalSince1970 + seconds) * 1000) }

    func testResetCountdownsRoundUpToMinutes() {
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(2 * 3600 + 13 * 60 + 1), now: now), "resets in 2h 14m")
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(3600), now: now), "resets in 1h")
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(30), now: now), "resets in 1m")
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(3 * 86400 + 4 * 3600), now: now), "resets in 3d 4h")
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(2 * 86400), now: now), "resets in 2d")
        XCTAssertEqual(UsageFormat.resetsIn(ms: ms(-5), now: now), "resets now")
    }

    func testPercentsLevelsAndLabels() {
        XCTAssertEqual(UsageFormat.percent(0.644), "64%")
        XCTAssertEqual(UsageFormat.percent(1.3), "100%")
        XCTAssertEqual(UsageFormat.level(0.79), .normal)
        XCTAssertEqual(UsageFormat.level(0.8), .warn)
        XCTAssertEqual(UsageFormat.level(0.95), .critical)
        XCTAssertEqual(UsageFormat.windowLabel("Session"), "5-hour")
        XCTAssertEqual(UsageFormat.windowLabel("five_hour"), "5-hour")
        XCTAssertEqual(UsageFormat.windowLabel("Week"), "Weekly")
        XCTAssertEqual(UsageFormat.windowLabel("Month"), "Month")
    }

    func testTokensAndDollarsLikeTheDesktop() {
        XCTAssertEqual(UsageFormat.tokens(987), "987")
        XCTAssertEqual(UsageFormat.tokens(12_340), "12K")
        XCTAssertEqual(UsageFormat.tokens(1_200), "1.2K")
        XCTAssertEqual(UsageFormat.tokens(4_500_000), "4.5M")
        XCTAssertEqual(UsageFormat.tokens(1_200_000_000), "1.2B")
        XCTAssertEqual(UsageFormat.usd(0), "$0")
        XCTAssertEqual(UsageFormat.usd(0.001), "<$0.01")
        XCTAssertEqual(UsageFormat.usd(12.345), "$12.35")
        XCTAssertEqual(UsageFormat.usd(1234.4), "$1,234")
    }

    func testMomentLabels() {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "UTC")!
        let locale = Locale(identifier: "en_US")
        let base = cal.date(from: DateComponents(year: 2026, month: 10, day: 19, hour: 10))!
        let at = { (s: TimeInterval) in Int64((base.timeIntervalSince1970 + s) * 1000) }
        // Formatters put a narrow no-break space before AM/PM.
        let label = { (s: TimeInterval) in
            UsageFormat.moment(ms: at(s), now: base, calendar: cal, locale: locale).replacingOccurrences(of: "\u{202F}", with: " ")
        }
        XCTAssertEqual(label(5 * 3600 + 40 * 60), "3:40 PM")
        XCTAssertEqual(label(26 * 3600), "Tue 12:00 PM")
        XCTAssertEqual(label(10 * 86400), "Oct 29, 10:00 AM")
    }

    // MARK: Widgets

    private func snapshot(used: Double = 0.64, resetIn: TimeInterval? = 3600, fetchedAgo: TimeInterval = 60) -> UsageWidgetSnapshot {
        UsageWidgetSnapshot(
            agent: "Claude Code", harness: "claude-code", account: "dev@example.com",
            window: .init(label: "5-hour", kind: .fiveHour, used: used, resetsAtMs: resetIn.map(ms)),
            weekly: nil, fetchedAtMs: ms(-fetchedAgo)
        )
    }

    func testTimelineFlipsToResetThenStale() {
        let entries = UsageTimeline.entries(snapshot(), now: now)
        XCTAssertEqual(entries.map(\.state), [.live, .reset])
        XCTAssertEqual(entries[0].used, 0.64)
        XCTAssertEqual(entries[1].used, 0, "a reset window reads empty until the app reads it again")
        XCTAssertEqual(entries[1].date, Date(timeIntervalSince1970: now.timeIntervalSince1970 + 3600))
        XCTAssertGreaterThan(UsageTimeline.refreshDate(entries, now: now), entries[1].date)
    }

    func testTimelineGoesStaleWithoutAReset() {
        let entries = UsageTimeline.entries(snapshot(resetIn: nil, fetchedAgo: 3600), now: now)
        XCTAssertEqual(entries.map(\.state), [.live, .stale])
        XCTAssertEqual(entries[1].date.timeIntervalSince(now), UsageTimeline.staleAfter - 3600, accuracy: 1)
        XCTAssertEqual(entries[1].used, 0.64, "stale still shows the last reading")
    }

    func testTimelineWithoutASnapshotAsksToOpenTheApp() {
        let entries = UsageTimeline.entries(nil, now: now)
        XCTAssertEqual(entries.map(\.state), [.empty])
        XCTAssertEqual(UsageTimeline.refreshDate(entries, now: now).timeIntervalSince(now), 1800, accuracy: 1)
        XCTAssertEqual(UsageTimeline.state(snapshot(resetIn: -1), at: now), .reset)
    }

    func testWidgetFollowsTheActiveClaudeLoginsFiveHourWindow() {
        func window(_ label: String, _ kind: UsageWindowKind, _ used: Double) -> UsageWindowView {
            UsageWindowView(label: label, kind: kind, usedFraction: used, resetsAtMs: ms(600))
        }
        func account(_ id: String, _ harness: String, active: Bool, _ windows: [UsageWindowView]) -> AgentAccountView {
            AgentAccountView(id: id, harness: harness, harnessLabel: harness == "codex" ? "Codex" : "Claude Code", email: "\(id)@x.com", displayName: nil, planLabel: "Max", active: active, windows: windows, fetchedAtMs: ms(-30), error: nil)
        }
        let list = [
            account("codex", "codex", active: true, [window("5h", .fiveHour, 0.2)]),
            account("claude-old", "claude-code", active: false, [window("Session", .fiveHour, 0.99)]),
            account("claude", "claude-code", active: true, [window("Session", .fiveHour, 0.64), window("Week", .weekly, 0.38)]),
        ]
        let s = UsageFeed.widgetSnapshot(list, now: now)
        XCTAssertEqual(s?.account, "claude@x.com")
        XCTAssertEqual(s?.window.label, "5-hour")
        XCTAssertEqual(s?.window.used, 0.64)
        XCTAssertEqual(s?.weekly?.label, "Weekly")
        // No 5-hour window anywhere: the most-used one.
        let month = [account("cursor", "cursor", active: true, [window("Month", .other, 0.3), window("Premium", .other, 0.7)])]
        XCTAssertEqual(UsageFeed.widgetSnapshot(month, now: now)?.window.label, "Premium")
        XCTAssertNil(UsageFeed.widgetSnapshot([account("x", "codex", active: false, [window("5h", .fiveHour, 0.1)])], now: now))
    }

    func testLimitBannerCopy() {
        let reset = ms(47 * 60)
        let ask = LimitBanner.text(.init(resetAtMs: reset, window: "5-hour", recovery: .ask), now: now)
        XCTAssertTrue(ask.title.hasPrefix("Limited · resumes at "), ask.title)
        XCTAssertTrue(ask.detail.contains("the 5-hour limit") && ask.detail.contains("47m"), ask.detail)
        XCTAssertTrue(LimitBanner.text(.init(resetAtMs: reset, window: nil, recovery: .resumeAtReset), now: now).title.hasPrefix("Resumes at "))
        XCTAssertEqual(LimitBanner.text(.init(resetAtMs: nil, window: nil, recovery: .ask), now: now).title, "Limited")
    }
}
