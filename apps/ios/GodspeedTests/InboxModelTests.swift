import XCTest
@testable import Godspeed

final class InboxModelTests: XCTestCase {
    func testRanksByRiskBandThenOldestFirstAcrossHosts() {
        let items = [
            Fixture.item("finished", kind: .finished, minute: 0, canDismiss: true),
            Fixture.approval("low-new", risk: .low, minute: 9),
            Fixture.approval("low-old", device: "vps", risk: .low, minute: 2),
            Fixture.item("question", kind: .question, minute: 8),
            Fixture.approval("high", device: "vps", risk: .high, minute: 7),
            Fixture.approval("medium", risk: .medium, minute: 1),
            Fixture.item("failed", kind: .failed, minute: 3, canDismiss: true),
        ]
        let model = InboxModel(items: items)
        XCTAssertEqual(model.items.map(\.id), ["high", "question", "medium", "low-old", "low-new", "failed", "finished"])
        XCTAssertEqual(model.badgeCount, 7)
        XCTAssertEqual(model.lowRiskCount, 2)
    }

    func testHiddenRowsDropOutAndKeysAreUniquePerHost() {
        let a = Fixture.approval("same", device: "mac", risk: .low, minute: 0)
        let b = Fixture.approval("same", device: "vps", risk: .low, minute: 1)
        XCTAssertNotEqual(InboxModel.key(a), InboxModel.key(b))
        let model = InboxModel(items: [a, b], hidden: [InboxModel.key(a)])
        XCTAssertEqual(model.items.map(\.deviceId), ["vps"])
        XCTAssertEqual(model.item(key: InboxModel.key(b))?.deviceId, "vps")
        XCTAssertNil(model.item(key: InboxModel.key(a)))
    }

    func testSwipesApproveRightDeclineOrDismissLeft() {
        let approval = Fixture.approval("a", risk: .medium, minute: 0)
        XCTAssertEqual(InboxModel.swipes(approval).leading, .approve)
        XCTAssertEqual(InboxModel.swipes(approval).trailing, .decline)
        let finished = Fixture.item("f", kind: .finished, minute: 0, canDismiss: true)
        XCTAssertNil(InboxModel.swipes(finished).leading)
        XCTAssertEqual(InboxModel.swipes(finished).trailing, .dismiss)
        XCTAssertTrue(InboxModel.dismissesOnOpen(finished))
        // A host without a live inbox: open-only.
        let derived = Fixture.approval("d", risk: .low, minute: 0, derived: true)
        XCTAssertNil(InboxModel.swipes(derived).leading)
        XCTAssertNil(InboxModel.swipes(derived).trailing)
        XCTAssertEqual(InboxModel(items: [derived]).lowRiskCount, 0)
        let question = Fixture.item("q", kind: .question, minute: 0)
        XCTAssertNil(InboxModel.swipes(question).leading)
        XCTAssertNil(InboxModel.swipes(question).trailing)
    }

    func testDetailShowsTheExactCommandElseTheDiff() {
        XCTAssertEqual(InboxModel.detail(Fixture.approval("c", risk: .high, minute: 0, command: "git push --force")), "git push --force")
        let diff = (1...12).map { "+line \($0)" }.joined(separator: "\n")
        let shown = InboxModel.detail(Fixture.approval("d", risk: .medium, minute: 0, command: nil, diff: diff))
        XCTAssertEqual(shown?.split(separator: "\n").count, 8)
        XCTAssertNil(InboxModel.detail(Fixture.item("q", kind: .question, minute: 0)))
    }

    func testRiskColoursAndBadgeText() {
        XCTAssertEqual(InboxModel.riskColor(.low), Palette.success)
        XCTAssertEqual(InboxModel.riskColor(.medium), Palette.warning)
        XCTAssertEqual(InboxModel.riskColor(.high), Palette.danger)
        XCTAssertNil(InboxModel.badgeText(0))
        XCTAssertEqual(InboxModel.badgeText(7), "7")
        XCTAssertEqual(InboxModel.badgeText(140), "99+")
    }

    func testVoiceOverLabelReadsKindRiskAndCommand() {
        let label = InboxModel.accessibilityLabel(Fixture.approval("a", risk: .high, minute: 0, command: "rm -rf build"), now: Fixture.date(minute: 5))
        XCTAssertTrue(label.hasPrefix("Approval, High risk, Title a"), label)
        XCTAssertTrue(label.contains("Command: rm -rf build"), label)
        XCTAssertTrue(label.hasSuffix("5m ago"), label)
    }
}
