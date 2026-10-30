import XCTest

/// "Look around with sample data" on the sign-in screen: the path App Review
/// and testers without a Mac take into the offline demo workspace.
final class SampleDataTests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
        try XCTSkipIf(UIDevice.current.userInterfaceIdiom == .pad, "phone shell")
    }

    func testSampleDataOpensTheDemoWorkspace() {
        let app = XCUIApplication()
        // Debug-only hook: start signed out (no -demo, so the sign-in screen shows).
        app.launchArguments = ["-signedout", "-fast"]
        app.launch()
        let sample = app.buttons["sample-data"]
        XCTAssertTrue(sample.waitForExistence(timeout: 10))
        XCTAssertTrue(app.buttons["sign-in"].exists)
        sample.tap()
        XCTAssertTrue(app.staticTexts["Sessions"].waitForExistence(timeout: 10), "the demo workspace's Sessions list shows")
        XCTAssertFalse(app.buttons["sign-in"].exists)
    }
}
