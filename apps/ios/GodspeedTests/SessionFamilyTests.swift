import XCTest
@testable import Godspeed

/// The multi-session model behind the strip, the approval panel, the cards
/// and the list rollups (docs/godspeed/MOBILE-MULTI-SESSION.md).
final class SessionFamilyTests: XCTestCase {
    /// A conductor with four sub-agents (one on another host, one with a
    /// sub-session of its own, one archived) and a plain chat.
    private let rows: [SessionRow] = [
        Fixture.row("main", indicator: .working, lastMinute: -1, workingSinceMinute: -30),
        Fixture.row("ui", indicator: .awaitingApproval, lastMinute: -2, parent: "main"),
        Fixture.row("sync", indicator: .working, lastMinute: -1, workingSinceMinute: -25, parent: "main"),
        Fixture.row("probe", indicator: .working, lastMinute: -1, parent: "sync"),
        Fixture.row("tests", device: "vps", deviceName: "hetzner-01", indicator: .completed, lastMinute: -4, parent: "main"),
        Fixture.row("docs", indicator: .errored, lastMinute: -7, parent: "main"),
        Fixture.row("old", indicator: .idle, lastMinute: -90, archived: true, parent: "main"),
        Fixture.row("plain", indicator: .idle),
    ]

    private func family(_ id: String, rows: [SessionRow]? = nil, attention: [AttentionItemView] = []) -> SessionFamily? {
        let all = rows ?? self.rows
        let byId = Dictionary(all.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        return SessionFamily.build(
            currentId: id,
            row: { byId[$0] },
            children: { parent in all.filter { $0.parentChatId == parent } },
            attention: attention
        )
    }

    private var asks: [AttentionItemView] {
        [
            Fixture.approval("r-ui", risk: .medium, minute: -2, chat: "ui", command: "xcodebuild test"),
            Fixture.approval("r-probe", device: "vps", risk: .high, minute: -1, chat: "probe"),
            Fixture.item("done", kind: .finished, minute: -3),
        ]
    }

    func testAPlainChatHasNoFamily() {
        XCTAssertNil(family("plain"))
        XCTAssertNil(family("missing"))
    }

    func testTheParentListsItsSubSessionsWaitingOnYouFirst() throws {
        let f = try XCTUnwrap(family("main", attention: asks))
        XCTAssertTrue(f.rootIsCurrent)
        XCTAssertNil(f.up)
        // Archived children are left out; a grandchild's approval lights its
        // parent; then failed before done.
        XCTAssertEqual(f.members.map(\.id), ["sync", "ui", "docs", "tests"])
        XCTAssertEqual(f.members.map(\.state), [.needsYou, .needsYou, .failed, .done])
        let sync = f.members[0]
        XCTAssertEqual(sync.nested, 1)
        XCTAssertEqual(sync.waiting, 1)
        XCTAssertEqual(sync.activity, "Below: Approve: Run the tests")
        XCTAssertEqual(f.members[1].activity, "Approve: Run the tests")
        XCTAssertEqual(f.members[3].host, "hetzner-01", "a sub-session on another host names it")
        XCTAssertNil(f.members[1].host)
        // Everything below the page waits on you, inbox order (high risk first);
        // finished rows aren't asks.
        XCTAssertEqual(f.waiting.map(\.id), ["r-probe", "r-ui"])
        XCTAssertEqual(f.root.waiting, 2)
        XCTAssertEqual(f.chips.map(\.id), ["root:main", "sync", "ui", "docs", "tests"])
        guard case let .root(_, selected) = f.chips[0] else { return XCTFail("root first") }
        XCTAssertTrue(selected)
    }

    func testASubSessionShowsItsSiblingsWithTheParentAsBreadcrumb() throws {
        let f = try XCTUnwrap(family("docs", attention: asks))
        XCTAssertFalse(f.rootIsCurrent)
        XCTAssertEqual(f.root.id, "main")
        XCTAssertEqual(f.chips.first?.id, "root:main")
        guard case let .root(_, rootSelected) = f.chips[0] else { return XCTFail("root first") }
        XCTAssertFalse(rootSelected, "the parent is the way back, not the page")
        let selected = f.chips.compactMap { chip -> String? in
            if case let .member(m, true) = chip { return m.id }
            return nil
        }
        XCTAssertEqual(selected, ["docs"])
        XCTAssertTrue(f.waiting.isEmpty, "nothing below docs waits")
    }

    func testAMiddleSessionListsItsOwnSubSessionsWithABreadcrumbUp() throws {
        let f = try XCTUnwrap(family("sync", attention: asks))
        XCTAssertTrue(f.rootIsCurrent)
        XCTAssertEqual(f.up?.id, "main")
        XCTAssertEqual(f.chips.map(\.id), ["up:main", "root:sync", "probe"])
        XCTAssertEqual(f.waiting.map(\.id), ["r-probe"])
        XCTAssertNil(f.members.first?.host, "same host as its parent")
    }

    func testManySubSessionsEndTheStripWithAll() throws {
        var rows = [Fixture.row("main", indicator: .working)]
        for i in 0..<12 { rows.append(Fixture.row("c\(i)", indicator: i % 3 == 0 ? .working : .completed, lastMinute: Int64(-i), parent: "main")) }
        let f = try XCTUnwrap(family("main", rows: rows))
        XCTAssertEqual(f.members.count, 12)
        XCTAssertEqual(f.chips.last?.id, "#all")
        XCTAssertEqual(f.chips.count, 1 + 12 + 1)
        XCTAssertEqual(f.members.prefix(4).map(\.state), Array(repeating: .working, count: 4), "live work before finished")
    }

    func testBadParentLinksCantLoop() throws {
        // a ⇄ b, and a long chain: the walk is bounded either way.
        var rows = [
            Fixture.row("a", indicator: .working, parent: "b"),
            Fixture.row("b", indicator: .working, parent: "a"),
        ]
        rows.append(Fixture.row("n0", indicator: .working))
        for i in 1..<12 { rows.append(Fixture.row("n\(i)", indicator: .working, parent: "n\(i - 1)")) }
        let cyclic = try XCTUnwrap(family("a", rows: rows))
        XCTAssertEqual(cyclic.members.map(\.id), ["b"])
        let chain = try XCTUnwrap(family("n0", rows: rows))
        XCTAssertEqual(chain.members.first?.nested, SessionFamily.maxDepth, "the walk stops at the depth cap")
        let all = Dictionary(grouping: rows, by: { $0.parentChatId ?? "" })
        let rollup = SessionFamily.rollup("a", children: { all[$0] ?? [] }, attention: [])
        XCTAssertEqual(rollup.children, 1)
    }

    func testRowsRollUpSubSessionsAndWhatWaitsBelow() {
        let byParent = Dictionary(grouping: rows, by: { $0.parentChatId ?? "" })
        let main = SessionFamily.rollup("main", children: { byParent[$0] ?? [] }, attention: [])
        XCTAssertEqual(main, SessionFamily.Rollup(children: 4, needsYou: true))
        let sync = SessionFamily.rollup("sync", children: { byParent[$0] ?? [] }, attention: [])
        XCTAssertEqual(sync, SessionFamily.Rollup(children: 1, needsYou: false))
        // A question on a grandchild counts too.
        let asked = SessionFamily.rollup("sync", children: { byParent[$0] ?? [] }, attention: [Fixture.approval("q", risk: .low, minute: 0, chat: "probe")])
        XCTAssertTrue(asked.needsYou)
        XCTAssertEqual(SessionFamily.rollup("plain", children: { byParent[$0] ?? [] }, attention: []), SessionFamily.Rollup())
    }

    func testPreviewsBecomeOneCleanLine() {
        XCTAssertEqual(SessionFamily.clean("**Tests** pass:\n`9/9`  Running…"), "Tests pass: 9/9")
        XCTAssertEqual(SessionFamily.clean("## Outline"), "Outline")
    }

    func testStatusLinesAndDetails() throws {
        let f = try XCTUnwrap(family("main", attention: asks))
        let now = Fixture.date(minute: 0)
        let sync = try XCTUnwrap(f.members.first { $0.id == "sync" })
        XCTAssertEqual(sync.statusLine(now: now), "Needs you · 1m ago")
        let docs = try XCTUnwrap(f.members.first { $0.id == "docs" })
        XCTAssertEqual(docs.statusLine(now: now), "Failed · 7m ago")
        let tests = try XCTUnwrap(f.members.first { $0.id == "tests" })
        XCTAssertTrue(tests.details(now: now).hasPrefix("on hetzner-01"))
        XCTAssertTrue(sync.details(now: now).contains("1 session below"))
    }

    func testSubSessionAsksLightTheirTopLevelCardOnMissionControl() {
        let board = BoardModel.build(
            rows: rows.filter { $0.parentChatId == nil },
            projects: [],
            devices: [Fixture.device("mac", name: "MacBook Pro")],
            attention: asks,
            now: Fixture.date(minute: 0),
            ancestors: ["ui": "main", "sync": "main", "probe": "main", "tests": "main", "docs": "main"]
        )
        let card = board.devices.flatMap { $0.projects.flatMap(\.cards) }.first { $0.id == "main" }
        XCTAssertEqual(card?.status, .needsYou)
        XCTAssertEqual(card?.activity, "Run the tests")
        XCTAssertEqual(board.totals.needsYou, 1)
    }
}
