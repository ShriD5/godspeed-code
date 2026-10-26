import AVFoundation
import Speech
import UIKit

// MARK: - Insertion (pure)

/// Where dictated words land in the composer: at the cursor when dictation
/// started, with the text around it kept. Results arrive per recognition
/// segment (a server task is restarted about every minute); each segment's
/// partial replaces its previous partial, and finals stay put.
///
/// All ranges are UTF-16 (NSString) offsets, like UITextView's.
struct DictationInsertion: Equatable {
    struct Edit: Equatable {
        /// Range in the current text to replace (what was shown before).
        let range: NSRange
        let replacement: String
        /// Caret after the edit: just past the dictated words.
        let cursor: Int
    }

    /// Insertion point in the text.
    let anchor: Int
    /// Characters either side of the insertion point (nil at the ends), for
    /// the separating spaces.
    private let before: unichar?
    private let after: unichar?
    private(set) var segments: [Int: String] = [:]
    /// What the text currently holds at `anchor` (separators included).
    private(set) var shown = ""

    /// Starts at the cursor (the end of a selection: selected text is kept).
    init(text: String, selection: NSRange) {
        let ns = text as NSString
        let location = min(max(0, selection.location + selection.length), ns.length)
        anchor = location
        before = location > 0 ? ns.character(at: location - 1) : nil
        after = location < ns.length ? ns.character(at: location) : nil
    }

    /// Dictated words joined across segments, without separators.
    var words: String {
        segments.keys.sorted().compactMap { key in
            let s = segments[key]!.trimmingCharacters(in: .whitespacesAndNewlines)
            return s.isEmpty ? nil : s
        }.joined(separator: " ")
    }

    /// A segment's latest transcript (partial or final). Nil when nothing
    /// visible changes.
    mutating func update(segment: Int, transcript: String) -> Edit? {
        segments[segment] = transcript
        let words = self.words
        var next = ""
        if !words.isEmpty {
            if let before, !Self.isSpace(before) { next += " " }
            next += words
            if let after, !Self.isSpace(after), !Self.isClosingPunctuation(after) { next += " " }
        }
        guard next != shown else { return nil }
        let edit = Edit(
            range: NSRange(location: anchor, length: (shown as NSString).length),
            replacement: next,
            cursor: anchor + Self.cursorOffset(in: next, hasTrailingSpace: after.map { !Self.isSpace($0) && !Self.isClosingPunctuation($0) } ?? false)
        )
        shown = next
        return edit
    }

    /// The full text after applying `edit` (tests and the fallback path).
    static func apply(_ edit: Edit, to text: String) -> String {
        (text as NSString).replacingCharacters(in: edit.range, with: edit.replacement)
    }

    /// True while the text still holds exactly what dictation put there (the
    /// user hasn't edited around it).
    func isIntact(in text: String) -> Bool {
        let ns = text as NSString
        let length = (shown as NSString).length
        guard anchor + length <= ns.length else { return false }
        return ns.substring(with: NSRange(location: anchor, length: length)) == shown
    }

    private static func cursorOffset(in shown: String, hasTrailingSpace: Bool) -> Int {
        let length = (shown as NSString).length
        return hasTrailingSpace && length > 0 ? length - 1 : length
    }

    private static func isSpace(_ c: unichar) -> Bool {
        guard let scalar = Unicode.Scalar(c) else { return false }
        return CharacterSet.whitespacesAndNewlines.contains(scalar)
    }

    private static func isClosingPunctuation(_ c: unichar) -> Bool {
        guard let scalar = Unicode.Scalar(c) else { return false }
        return ".,;:!?)]}'\"".unicodeScalars.contains(scalar)
    }
}

// MARK: - Button state (pure)

/// The composer's mic button: what it shows and what a tap does.
struct DictationButtonModel: Equatable {
    enum Phase: Equatable {
        case idle
        /// Permissions / audio coming up; a tap cancels.
        case starting
        case listening
    }

    enum TapAction: Equatable { case start, stop, none }

    let symbol: String
    /// Filled, accent-coloured "live" look.
    let live: Bool
    let enabled: Bool
    let accessibilityLabel: String

    init(phase: Phase, voiceCallLive: Bool) {
        switch phase {
        case .idle:
            symbol = "mic"
            live = false
            // The Codex voice call owns the microphone.
            enabled = !voiceCallLive
            accessibilityLabel = "Dictate"
        case .starting, .listening:
            symbol = "mic.fill"
            live = true
            enabled = true
            accessibilityLabel = "Stop dictation"
        }
    }

    static func tapAction(phase: Phase, voiceCallLive: Bool) -> TapAction {
        switch phase {
        case .idle: voiceCallLive ? .none : .start
        case .starting, .listening: .stop
        }
    }

    /// A voice call starting mid-dictation takes the microphone.
    static func shouldStop(phase: Phase, voiceCallLive: Bool) -> Bool {
        phase != .idle && voiceCallLive
    }
}

// MARK: - Rewrite guard (pure)

/// The composer must never replace `text`/`attributedText` while iOS is
/// dictating into it (keyboard mic) or while our own dictation is streaming:
/// a wholesale replace ends system dictation and drops the words in flight.
enum ComposerRewriteGuard {
    static func canRewrite(systemDictating: Bool, ownDictating: Bool, hasMarkedText: Bool) -> Bool {
        !systemDictating && !ownDictating && !hasMarkedText
    }
}

// MARK: - Speech

/// Speech → text for the composer's mic button. On-device recognition when
/// the locale supports it (no time limit); otherwise Apple's server, whose
/// ~1 minute task limit is hidden by rolling over to a fresh task that keeps
/// consuming the same microphone stream.
@MainActor
final class DictationController {
    enum Failure: Equatable {
        /// Microphone or speech recognition permission denied / restricted.
        case denied
        /// No recognizer for this locale, or it's offline.
        case unavailable
        /// The microphone couldn't start (busy, no input).
        case audio
    }

    private(set) var phase: DictationButtonModel.Phase = .idle {
        didSet { if phase != oldValue { onPhase?(phase) } }
    }

    var onPhase: ((DictationButtonModel.Phase) -> Void)?
    /// Segment index, its latest transcript, whether it is final.
    var onTranscript: ((Int, String, Bool) -> Void)?
    /// 0…1 microphone level, ~15 times a second.
    var onLevel: ((Float) -> Void)?
    var onFailure: ((Failure) -> Void)?

    private let engine = AVAudioEngine()
    private let feed = AudioFeed()
    private var recognizer: SFSpeechRecognizer?
    private var onDevice = false
    private var tasks: [Int: SFSpeechRecognitionTask] = [:]
    private var segment = 0
    private var segmentStarted = Date()
    private var segmentHadResult = false
    private var quickFailures = 0
    private var rollover: Timer?
    private var generation = 0
    private var observers: [NSObjectProtocol] = []
    private var tapInstalled = false

    /// Server tasks stop at about a minute; roll over a little before.
    private static let serverSegment: TimeInterval = 50

    func start() {
        guard phase == .idle else { return }
        phase = .starting
        generation &+= 1
        let generation = self.generation
        Task { @MainActor [weak self] in
            guard let self else { return }
            let allowed = await Self.requestPermissions()
            guard self.generation == generation, self.phase == .starting else { return }
            guard allowed else { return self.fail(.denied) }
            guard let recognizer = Self.makeRecognizer() else { return self.fail(.unavailable) }
            self.recognizer = recognizer
            self.onDevice = recognizer.supportsOnDeviceRecognition
            do {
                try self.startAudio()
            } catch {
                return self.fail(.audio)
            }
            self.phase = .listening
            self.quickFailures = 0
            self.beginSegment()
        }
    }

    /// Stops listening. Words already shown stay; nothing arrives afterwards.
    func stop() {
        guard phase != .idle else { return }
        generation &+= 1
        teardown()
        phase = .idle
    }

    private func fail(_ failure: Failure) {
        generation &+= 1
        teardown()
        phase = .idle
        onFailure?(failure)
    }

    // MARK: Permissions & recognizer

    private static func requestPermissions() async -> Bool {
        let speech: SFSpeechRecognizerAuthorizationStatus
        switch SFSpeechRecognizer.authorizationStatus() {
        case .notDetermined:
            speech = await withCheckedContinuation { continuation in
                SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
            }
        case let status:
            speech = status
        }
        guard speech == .authorized else { return false }
        return await AVAudioApplication.requestRecordPermission()
    }

    /// The user's language; US English when it has no recognizer or the
    /// recognizer can't run right now (offline without on-device support).
    private static func makeRecognizer() -> SFSpeechRecognizer? {
        for locale in [Locale.current, Locale(identifier: "en-US")] {
            guard let r = SFSpeechRecognizer(locale: locale) else { continue }
            if r.isAvailable || r.supportsOnDeviceRecognition { return r }
        }
        return nil
    }

    // MARK: Audio

    private func startAudio() throws {
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(.record, mode: .measurement, options: [.allowBluetoothHFP])
        try session.setActive(true, options: [])
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else { throw Failure.audio }
        let feed = self.feed
        let onLevel: (Float) -> Void = { [weak self] level in self?.onLevel?(level) }
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
            feed.append(buffer)
            if let level = feed.level(of: buffer) {
                DispatchQueue.main.async { onLevel(level) }
            }
        }
        tapInstalled = true
        engine.prepare()
        try engine.start()
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { [weak self] note in
            // A phone call, Siri or an alarm takes the microphone: stop, keep the words.
            guard (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt) == AVAudioSession.InterruptionType.began.rawValue else { return }
            MainActor.assumeIsolated { self?.stop() }
        })
        observers.append(center.addObserver(forName: AVAudioSession.mediaServicesWereResetNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.stop() }
        })
        observers.append(center.addObserver(forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main) { [weak self] _ in
            // The app keeps background audio for calls; dictation never records unseen.
            MainActor.assumeIsolated { self?.stop() }
        })
    }

    private func teardown() {
        rollover?.invalidate()
        rollover = nil
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers.removeAll()
        if engine.isRunning { engine.stop() }
        if tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        feed.swap(nil)?.endAudio()
        for task in tasks.values { task.cancel() }
        tasks.removeAll()
        recognizer = nil
        // Hand the audio back (music resumes); never hold a session idle.
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    // MARK: Segments

    private func beginSegment() {
        guard phase == .listening, let recognizer else { return }
        segment += 1
        let index = segment
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        request.requiresOnDeviceRecognition = onDevice
        request.addsPunctuation = true
        request.taskHint = .dictation
        let previous = feed.swap(request)
        // The previous segment hears no more audio and finalizes on its own.
        previous?.endAudio()
        segmentStarted = Date()
        segmentHadResult = false
        let generation = self.generation
        tasks[index] = recognizer.recognitionTask(with: request) { [weak self] result, error in
            let text = result?.bestTranscription.formattedString
            let isFinal = result?.isFinal ?? false
            let failed = error != nil
            DispatchQueue.main.async {
                self?.handle(index, generation: generation, text: text, isFinal: isFinal, failed: failed)
            }
        }
        rollover?.invalidate()
        rollover = nil
        if !onDevice {
            rollover = Timer.scheduledTimer(withTimeInterval: Self.serverSegment, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.beginSegment() }
            }
        }
    }

    private func handle(_ index: Int, generation: Int, text: String?, isFinal: Bool, failed: Bool) {
        guard generation == self.generation, phase == .listening else { return }
        if let text {
            if index == segment { segmentHadResult = true; quickFailures = 0 }
            onTranscript?(index, text, isFinal)
        }
        guard isFinal || failed else { return }
        tasks[index] = nil
        // Only the live segment ending on its own (time limit, long silence,
        // a dropped connection) needs a successor.
        guard index == segment else { return }
        if failed, !segmentHadResult, Date().timeIntervalSince(segmentStarted) < 2 {
            quickFailures += 1
            if quickFailures >= 3 { return fail(onDevice ? .audio : .unavailable) }
        }
        beginSegment()
    }
}

/// Hands microphone buffers from the audio thread to the live request.
private final class AudioFeed: @unchecked Sendable {
    private let lock = NSLock()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var counter = 0

    /// Installs `next`, returning the request it replaces.
    func swap(_ next: SFSpeechAudioBufferRecognitionRequest?) -> SFSpeechAudioBufferRecognitionRequest? {
        lock.lock(); defer { lock.unlock() }
        let previous = request
        request = next
        return previous
    }

    func append(_ buffer: AVAudioPCMBuffer) {
        lock.lock()
        let request = self.request
        lock.unlock()
        request?.append(buffer)
    }

    /// RMS level mapped to 0…1 (-50…-10 dBFS), every fourth buffer.
    func level(of buffer: AVAudioPCMBuffer) -> Float? {
        counter &+= 1
        guard counter % 4 == 0, let data = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return nil }
        let n = Int(buffer.frameLength)
        var sum: Float = 0
        for i in 0..<n { sum += data[i] * data[i] }
        let rms = (sum / Float(n)).squareRoot()
        let db = 20 * log10(max(rms, 1e-7))
        return min(1, max(0, (db + 50) / 40))
    }
}

extension DictationController.Failure: Error {}
