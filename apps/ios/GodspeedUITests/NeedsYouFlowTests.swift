import XCTest

/// The phone's Needs You inbox, Mission Control, Usage, the shelves and turn
/// receipts over the demo workspace (its hosts serve inbox queues, plan
/// windows and usage history; no network).
final class NeedsYouFlowTests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    private func launch(_ args: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-demo", "-fast"] + args
        app.launch()
        return app
    }

    func testInboxRanksAnswersAndApprovesLowRisk() {
        let app = launch(["-route", "inbox"])
        let high = app.cells["inbox-approval:chat-veil:r1"]
        let question = app.cells["inbox-chat:chat-picker"]
        XCTAssertTrue(high.waitForExistence(timeout: 10))
        XCTAssertTrue(question.exists)
        // High risk sits above the question.
        XCTAssertLessThan(high.frame.minY, question.frame.minY)
        XCTAssertTrue(high.label.contains("High risk") && high.label.contains("git push --force-with-lease"), high.label)
        // Swipe right approves (a full swipe, or the revealed action).
        high.swipeRight()
        let revealed = app.buttons["Approve"]
        if high.exists, revealed.waitForExistence(timeout: 1) { revealed.tap() }
        XCTAssertTrue(high.waitForNonExistence(timeout: 5))
        // One tap clears every low-risk approval.
        let all = app.buttons["approve-all-low-risk"]
        XCTAssertTrue(all.waitForExistence(timeout: 5))
        XCTAssertTrue(all.label.contains("2"), all.label)
        all.tap()
        XCTAssertTrue(all.waitForNonExistence(timeout: 5), "no low-risk approvals left")
        XCTAssertTrue(app.cells["inbox-approval:chat-ios-scroll:r2"].exists, "medium risk stays for you")
        // Tapping opens the chat.
        app.cells["inbox-chat:chat-picker"].tap()
        XCTAssertTrue(app.textViews["composer-input"].waitForExistence(timeout: 10) || app.buttons["question-option-0"].waitForExistence(timeout: 5))
    }

    func testMissionControlGroupsAgentsByDevice() {
        let app = launch(["-route", "board"])
        let limited = app.cells["board-chat-limited"]
        XCTAssertTrue(limited.waitForExistence(timeout: 10))
        XCTAssertTrue(app.descendants(matching: .any)["board-totals"].exists)
        XCTAssertTrue(app.cells.matching(NSPredicate(format: "label BEGINSWITH 'hetzner-01'")).firstMatch.exists, "grouped under its device")
        XCTAssertTrue(limited.label.contains("Limited · back in"), limited.label)
        XCTAssertTrue(app.cells["board-chat-errored"].label.contains("Failed"))
        // Every device's agents, on one board.
        let list = app.collectionViews["board-list"]
        let picker = app.cells["board-chat-picker"]
        for _ in 0..<5 where !picker.exists { list.swipeUp() }
        XCTAssertTrue(picker.exists)
        XCTAssertTrue(picker.label.contains("Needs you"), picker.label)
        picker.tap()
        XCTAssertTrue(app.textViews["composer-input"].waitForExistence(timeout: 10) || app.buttons["question-option-0"].waitForExistence(timeout: 5))
    }

    func testUsageShowsPlanWindowsAndHistory() {
        let app = launch(["-route", "usage"])
        XCTAssertTrue(app.otherElements["usage-account-claude-1"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.staticTexts["Est. cost at API prices"].waitForExistence(timeout: 10) || app.otherElements["Est. cost at API prices"].exists
            || app.descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH 'Est. cost at API prices'")).firstMatch.exists)
        let meter = app.descendants(matching: .any).matching(NSPredicate(format: "label == '5-hour window'")).firstMatch
        XCTAssertTrue(meter.exists)
        XCTAssertTrue((meter.value as? String ?? "").contains("64% used"))
    }

    func testLimitedChatOffersResumeAtReset() {
        let app = launch(["-route", "chat:chat-limited"])
        let resume = app.buttons["limit-primary"]
        XCTAssertTrue(resume.waitForExistence(timeout: 10))
        XCTAssertEqual(resume.label, "Resume at reset")
        resume.tap()
        XCTAssertTrue(app.buttons["Cancel"].waitForExistence(timeout: 5), "scheduled: the banner offers to cancel")
    }

    func testShelvesHoldSnoozedAndSettledSessions() {
        let app = launch([])
        XCTAssertTrue(app.staticTexts["Sessions"].waitForExistence(timeout: 10))
        let snoozed = app.cells["section-snoozed"]
        let list = app.collectionViews.firstMatch
        for _ in 0..<6 where !snoozed.exists { list.swipeUp() }
        XCTAssertTrue(snoozed.exists)
        XCTAssertTrue(app.cells["section-settled"].exists)
        // The shelf folds like any section (it starts folded on a fresh install).
        if snoozed.value as? String == "Collapsed" { snoozed.tap() }
        XCTAssertTrue(app.cells["session-chat-snoozed"].waitForExistence(timeout: 5))
        snoozed.tap()
        XCTAssertTrue(app.cells["session-chat-snoozed"].waitForNonExistence(timeout: 5))
        XCTAssertFalse(app.cells["session-chat-snoozed"].exists)
    }

    func testFinishedTurnShowsAReceipt() {
        let app = launch(["-route", "chat:chat-deploy"])
        let input = app.textViews["composer-input"]
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        input.tap()
        input.typeText("Summarize the launch post.")
        app.buttons["composer-send"].tap()
        let receipt = app.descendants(matching: .any).matching(identifier: "row-receipt").firstMatch
        XCTAssertTrue(receipt.waitForExistence(timeout: 40))
        XCTAssertTrue(receipt.label.hasPrefix("Receipt"), receipt.label)
    }
}
