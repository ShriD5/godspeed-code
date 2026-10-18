import XCTest
@testable import Godspeed

final class BoardModelTests: XCTestCase {
    private var utc: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        return c
    }

    func testGroupsByDeviceThenRepositoryAndSortsMostUrgentFirst() {
        let rows = [
            Fixture.row("working", indicator: .working, lastMinute: -1, workingSinceMinute: -4, branch: "veil-fade", subagents: 2),
            Fixture.row("waiting", indicator: .awaitingApproval, lastMinute: -2),
            Fixture.row("clone", project: "space-godspeed-2", indicator: .completed, lastMinute: -30),
            Fixture.row("vps-failed", device: "vps", deviceName: "hetzner-01", project: "space-edge", indicator: .errored, lastMinute: -10),
            Fixture.row("home", project: nil, indicator: .limited, lastMinute: -60, limitResetMinute: 47),
            Fixture.row("idle-old", indicator: .idle, lastMinute: -60 * 30),
            Fixture.row("settled", indicator: .completed, lastMinute: -5, lifecycle: .settled),
            Fixture.row("child", indicator: .working, parent: "working"),
            Fixture.row("archived", indicator: .working, archived: true),
        ]
        let projects = [
            Fixture.project("space-godspeed", name: "godspeed", group: "repo:godspeed"),
            Fixture.project("space-godspeed-2", name: "godspeed", group: "repo:godspeed"),
            Fixture.project("space-edge", name: "edge", device: "vps", group: "repo:edge"),
        ]
        let devices = [Fixture.device("mac", name: "MacBook Pro"), Fixture.device("vps", name: "hetzner-01", online: false)]
        let board = BoardModel.build(rows: rows, projects: projects, devices: devices, attention: [], now: Fixture.date(minute: 0), calendar: utc)

        XCTAssertEqual(board.devices.map(\.name), ["MacBook Pro", "hetzner-01"], "online hosts first")
        let mac = board.devices[0]
        // Clones share one repository group; projectless chats go under "No project".
        XCTAssertEqual(mac.projects.map(\.name), ["godspeed", "No project"])
        XCTAssertEqual(mac.projects[0].cards.map(\.id), ["waiting", "working", "clone"])
        XCTAssertEqual(mac.projects[1].cards.map(\.id), ["home"])
        XCTAssertEqual(board.devices[1].projects[0].cards.map(\.id), ["vps-failed"])
        let all = board.devices.flatMap { $0.projects.flatMap(\.cards) }.map(\.id)
        XCTAssertFalse(all.contains("idle-old"), "finished over a day ago drops off")
        XCTAssertFalse(all.contains("settled"), "put away and not live")
        XCTAssertFalse(all.contains("child") || all.contains("archived"))
        // "clone" finished half an hour ago, the same UTC day.
        XCTAssertEqual(board.totals, BoardModel.Totals(working: 1, needsYou: 1, limited: 1, finishedToday: 1))
    }

    func testStatusLabelsMatchTheDesktop() {
        let now = Fixture.date(minute: 0)
        func card(_ row: SessionRow, attention: [AttentionItemView] = []) -> BoardModel.Card {
            BoardModel.build(rows: [row], projects: [], devices: [], attention: attention, now: now, calendar: utc).devices[0].projects[0].cards[0]
        }
        XCTAssertEqual(card(Fixture.row("w", indicator: .working, workingSinceMinute: -65)).statusLabel, "Working · 1h 5m")
        XCTAssertEqual(card(Fixture.row("w", indicator: .working, workingSinceMinute: 0)).statusLabel, "Working · <1m")
        XCTAssertEqual(card(Fixture.row("b", indicator: .idle, backgroundWork: 3)).statusLabel, "Waiting on 3 tasks")
        XCTAssertEqual(card(Fixture.row("l", indicator: .limited, limitResetMinute: 47)).statusLabel, "Limited · back in 47m")
        XCTAssertEqual(card(Fixture.row("f", indicator: .errored, lastMinute: -10)).statusLabel, "Failed · 10m ago")
        XCTAssertEqual(card(Fixture.row("d", indicator: .completed, lastMinute: 0)).statusLabel, "Finished · just now")
        let ask = Fixture.approval("a", risk: .low, minute: -2, chat: "n")
        let needs = card(Fixture.row("n", indicator: .awaitingApproval, lastMinute: -9), attention: [ask])
        XCTAssertEqual(needs.statusLabel, "Needs you · 2m ago")
        XCTAssertEqual(needs.activity, "Run the tests", "the pending request, not the last message")
        XCTAssertEqual(needs.approvableKey, InboxModel.key(ask))
        let sub = card(Fixture.row("s", indicator: .working, workingSinceMinute: -1, branch: " main ", subagents: 3))
        XCTAssertEqual(sub.subagents, 3)
        XCTAssertEqual(sub.branch, "main")
    }

    func testFinishedTodayCountsFromLocalMidnight() {
        let now = Fixture.date(minute: 0)
        let midnight = utc.startOfDay(for: now)
        let minutesSinceMidnight = Int64(now.timeIntervalSince(midnight) / 60)
        let rows = [
            Fixture.row("today", indicator: .idle, lastMinute: -1),
            Fixture.row("yesterday", indicator: .idle, lastMinute: -(minutesSinceMidnight + 5)),
        ]
        let board = BoardModel.build(rows: rows, projects: [], devices: [], attention: [], now: now, calendar: utc)
        XCTAssertEqual(board.totals.finishedToday, 1)
    }
}
