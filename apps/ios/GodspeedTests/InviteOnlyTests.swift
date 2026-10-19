import XCTest
@testable import Godspeed

final class InviteOnlyTests: XCTestCase {
    private let notice = "Godspeed Code cloud sync is invite-only for now. Godspeed Code works fully without it, or you can host your own sync server: see edge/DEPLOY.md."

    func testInviteOnlyRefusalShowsTheEdgeMessage() {
        XCTAssertEqual(AppModel.signInErrorText(CoreError.Auth(message: notice)), notice)
        XCTAssertEqual(AppModel.inviteOnlyMessage(notice), notice)
    }

    func testOtherAuthFailuresAreNotInviteOnly() {
        XCTAssertNil(AppModel.inviteOnlyMessage("401 Unauthorized: {\"error\":\"revoked\"}"))
        XCTAssertNotEqual(AppModel.signInErrorText(CoreError.Auth(message: "session expired")), notice)
    }
}
