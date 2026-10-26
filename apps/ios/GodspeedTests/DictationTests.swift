import XCTest
@testable import Godspeed

/// The composer's dictation: where words land, what the mic button shows and
/// does, and that nothing rewrites the text while iOS dictates into it.
final class DictationTests: XCTestCase {
    // MARK: Insertion

    private func run(_ text: String, cursor: Int, _ steps: [(Int, String)]) -> (String, Int) {
        var insertion = DictationInsertion(text: text, selection: NSRange(location: cursor, length: 0))
        var current = text
        var caret = cursor
        for (segment, transcript) in steps {
            guard let edit = insertion.update(segment: segment, transcript: transcript) else { continue }
            current = DictationInsertion.apply(edit, to: current)
            caret = edit.cursor
            XCTAssertTrue(insertion.isIntact(in: current))
        }
        return (current, caret)
    }

    func testPartialsReplaceEachOtherInEmptyComposer() {
        let (text, caret) = run("", cursor: 0, [(1, "Fix"), (1, "Fix the"), (1, "Fix the build.")])
        XCTAssertEqual(text, "Fix the build.")
        XCTAssertEqual(caret, 14)
    }

    func testAppendsAfterExistingTextWithSpace() {
        let (text, caret) = run("Hello", cursor: 5, [(1, "world")])
        XCTAssertEqual(text, "Hello world")
        XCTAssertEqual(caret, 11)
    }

    func testNoDoubleSpaceAfterExistingSpace() {
        let (text, _) = run("Hello ", cursor: 6, [(1, "world")])
        XCTAssertEqual(text, "Hello world")
    }

    func testInsertsMidTextKeepingBothSides() {
        // "Fix | tests" → cursor after "Fix "
        let (text, caret) = run("Fix tests", cursor: 4, [(1, "the"), (1, "the failing")])
        XCTAssertEqual(text, "Fix the failing tests")
        XCTAssertEqual(caret, 15)
    }

    func testSeparatesFromFollowingWord() {
        let (text, caret) = run("Fixtests", cursor: 3, [(1, "the")])
        XCTAssertEqual(text, "Fix the tests")
        // Caret sits right after the dictated word, before the separator.
        XCTAssertEqual(caret, 7)
    }

    func testNoSpaceBeforeClosingPunctuation() {
        let (text, _) = run("Do it.", cursor: 5, [(1, "now")])
        XCTAssertEqual(text, "Do it now.")
    }

    func testSelectionIsKeptAndWordsGoAfterIt() {
        var insertion = DictationInsertion(text: "keep this", selection: NSRange(location: 5, length: 4))
        let edit = insertion.update(segment: 1, transcript: "too")!
        XCTAssertEqual(DictationInsertion.apply(edit, to: "keep this"), "keep this too")
    }

    func testSegmentsRollOverAndJoin() {
        // A server task finalizes at ~1 minute and a new one continues.
        let (text, _) = run("", cursor: 0, [
            (1, "First part"), (1, "First part of it."),
            (2, "Second"), (2, "Second part."),
        ])
        XCTAssertEqual(text, "First part of it. Second part.")
    }

    func testFinalOfOldSegmentArrivingAfterNewPartialStaysInOrder() {
        let (text, _) = run("", cursor: 0, [(1, "one two"), (2, "three"), (1, "One, two.")])
        XCTAssertEqual(text, "One, two. three")
    }

    func testEmptyPartialRemovesSeparator() {
        let (text, _) = run("Hi", cursor: 2, [(1, "there"), (1, "")])
        XCTAssertEqual(text, "Hi")
    }

    func testUnchangedTranscriptIsNoEdit() {
        var insertion = DictationInsertion(text: "", selection: NSRange(location: 0, length: 0))
        XCTAssertNotNil(insertion.update(segment: 1, transcript: "same"))
        XCTAssertNil(insertion.update(segment: 1, transcript: "same"))
    }

    func testUTF16OffsetsWithEmoji() {
        let (text, caret) = run("🚀 go", cursor: 5, [(1, "fast")])
        XCTAssertEqual(text, "🚀 go fast")
        XCTAssertEqual(caret, ("🚀 go fast" as NSString).length)
    }

    func testDetectsEditsUnderneath() {
        var insertion = DictationInsertion(text: "", selection: NSRange(location: 0, length: 0))
        let edit = insertion.update(segment: 1, transcript: "hello")!
        let text = DictationInsertion.apply(edit, to: "")
        XCTAssertTrue(insertion.isIntact(in: text))
        XCTAssertFalse(insertion.isIntact(in: "help"))
    }

    // MARK: Button

    func testButtonStates() {
        let idle = DictationButtonModel(phase: .idle, voiceCallLive: false)
        XCTAssertEqual(idle.symbol, "mic")
        XCTAssertFalse(idle.live)
        XCTAssertTrue(idle.enabled)
        XCTAssertEqual(idle.accessibilityLabel, "Dictate")

        let listening = DictationButtonModel(phase: .listening, voiceCallLive: false)
        XCTAssertEqual(listening.symbol, "mic.fill")
        XCTAssertTrue(listening.live)
        XCTAssertEqual(listening.accessibilityLabel, "Stop dictation")

        XCTAssertTrue(DictationButtonModel(phase: .starting, voiceCallLive: false).live)
        XCTAssertFalse(DictationButtonModel(phase: .idle, voiceCallLive: true).enabled)
    }

    func testTapActions() {
        XCTAssertEqual(DictationButtonModel.tapAction(phase: .idle, voiceCallLive: false), .start)
        XCTAssertEqual(DictationButtonModel.tapAction(phase: .idle, voiceCallLive: true), .none)
        XCTAssertEqual(DictationButtonModel.tapAction(phase: .starting, voiceCallLive: false), .stop)
        XCTAssertEqual(DictationButtonModel.tapAction(phase: .listening, voiceCallLive: false), .stop)
    }

    func testVoiceCallStopsDictation() {
        XCTAssertTrue(DictationButtonModel.shouldStop(phase: .listening, voiceCallLive: true))
        XCTAssertTrue(DictationButtonModel.shouldStop(phase: .starting, voiceCallLive: true))
        XCTAssertFalse(DictationButtonModel.shouldStop(phase: .idle, voiceCallLive: true))
        XCTAssertFalse(DictationButtonModel.shouldStop(phase: .listening, voiceCallLive: false))
    }

    // MARK: Rewrite guard

    func testRewriteGuard() {
        XCTAssertTrue(ComposerRewriteGuard.canRewrite(systemDictating: false, ownDictating: false, hasMarkedText: false))
        XCTAssertFalse(ComposerRewriteGuard.canRewrite(systemDictating: true, ownDictating: false, hasMarkedText: false))
        XCTAssertFalse(ComposerRewriteGuard.canRewrite(systemDictating: false, ownDictating: true, hasMarkedText: false))
        XCTAssertFalse(ComposerRewriteGuard.canRewrite(systemDictating: false, ownDictating: false, hasMarkedText: true))
    }

    @MainActor
    func testTextSetIsDeferredDuringSystemDictation() throws {
        guard let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene else { throw XCTSkip("No window scene in this test host") }
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 390, height: 844)
        defer { window.isHidden = true }
        let composer = ComposerBar(frame: CGRect(x: 0, y: 700, width: 390, height: 60))
        window.addSubview(composer)
        window.makeKeyAndVisible()
        composer.text = "draft"
        guard composer.textView.becomeFirstResponder() else { throw XCTSkip("No first responder in this test host") }
        composer.textView.simulateSystemDictation(true)
        XCTAssertFalse(composer.canRewriteText)

        composer.text = "replaced while dictating"
        XCTAssertEqual(composer.textView.text, "draft", "must not rewrite mid-dictation")
        XCTAssertEqual(composer.pendingText, "replaced while dictating")

        composer.textView.simulateSystemDictation(false)
        XCTAssertEqual(composer.textView.text, "replaced while dictating")
        XCTAssertNil(composer.pendingText)
        XCTAssertTrue(composer.canRewriteText)
        _ = composer.textView.resignFirstResponder()
    }

    @MainActor
    func testTextSetAppliesImmediatelyWhenNotDictating() {
        let composer = ComposerBar(frame: CGRect(x: 0, y: 0, width: 390, height: 60))
        composer.text = "hello"
        XCTAssertEqual(composer.textView.text, "hello")
        XCTAssertNil(composer.pendingText)
    }

    @MainActor
    func testComposerAppliesDictationAtEndWhenUnfocused() {
        let composer = ComposerBar(frame: CGRect(x: 0, y: 0, width: 390, height: 60))
        composer.text = "Ship it"
        composer.beginDictationInsertion()
        composer.applyDictation(segment: 1, transcript: "today")
        composer.applyDictation(segment: 1, transcript: "today please.")
        XCTAssertEqual(composer.textView.text, "Ship it today please.")
        composer.endDictationInsertion()
    }
}
