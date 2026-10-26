import UIKit

/// The composer's text view. Knows when iOS is dictating into it (the
/// keyboard's mic), so the composer can hold off anything that would replace
/// its text mid-dictation — a wholesale `text`/`attributedText` set ends
/// system dictation and drops the words still being recognized.
final class ComposerTextView: UITextView {
    /// Fires when system dictation starts or ends.
    var onDictationChange: ((Bool) -> Void)?

    /// iOS is dictating into this view right now.
    var isSystemDictating: Bool { dictating }

    private var dictating = false {
        didSet { if dictating != oldValue { onDictationChange?(dictating) } }
    }
    private var modeObserver: NSObjectProtocol?

    override init(frame: CGRect, textContainer: NSTextContainer?) {
        super.init(frame: frame, textContainer: textContainer)
        modeObserver = NotificationCenter.default.addObserver(forName: UITextInputMode.currentInputModeDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.refreshDictation() }
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    deinit {
        if let modeObserver { NotificationCenter.default.removeObserver(modeObserver) }
    }

    /// The keyboard switches its input mode to "dictation" while the mic is
    /// live (inline dictation keeps the keyboard up, so focus alone says nothing).
    func refreshDictation() {
        dictating = isFirstResponder && textInputMode?.primaryLanguage == "dictation"
    }

    @discardableResult
    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        refreshDictation()
        return became
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        refreshDictation()
        return resigned
    }

    #if DEBUG
    /// Unit tests: stand in for the keyboard's dictation mode.
    func simulateSystemDictation(_ on: Bool) { dictating = on }
    #endif
}
