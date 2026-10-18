import UIKit

/// Above the composer while a chat is stopped on a usage limit (the desktop's
/// limit banner): when the window reopens, and what happens then — resume
/// the same session at the reset, or snooze it out of the way until then.
/// The host owns the timer; the choice goes to it over the relay.
final class LimitBanner: UIView {
    struct State: Equatable {
        var resetAtMs: Int64?
        var window: String?
        var recovery: LimitRecoveryMode
    }

    /// Resume at the reset / snooze until it / cancel a scheduled choice.
    var onAction: ((LimitAction) -> Void)?

    private let glass = Glass.surface(radius: 22)
    private let icon = UIImageView(image: UIImage(systemName: "hourglass", withConfiguration: UIImage.SymbolConfiguration(pointSize: 15, weight: .semibold)))
    private let title = UILabel()
    private let detail = UILabel()
    private let primary = UIButton(type: .system)
    private let secondary = UIButton(type: .system)
    private(set) var state: State?

    override init(frame: CGRect) {
        super.init(frame: frame)
        glass.translatesAutoresizingMaskIntoConstraints = false
        addSubview(glass)
        icon.tintColor = StatusTone.limited
        icon.setContentHuggingPriority(.required, for: .horizontal)
        title.numberOfLines = 0
        title.textColor = Palette.text
        detail.numberOfLines = 0
        detail.textColor = Palette.secondary
        for (b, prominent) in [(primary, true), (secondary, false)] {
            var c: UIButton.Configuration = prominent ? .filled() : .tinted()
            c.cornerStyle = .capsule
            c.baseBackgroundColor = StatusTone.limited
            c.baseForegroundColor = prominent ? .white : StatusTone.limited
            c.contentInsets = NSDirectionalEdgeInsets(top: 7, leading: 14, bottom: 7, trailing: 14)
            b.configuration = c
        }
        primary.accessibilityIdentifier = "limit-primary"
        secondary.accessibilityIdentifier = "limit-secondary"
        let texts = UIStackView(arrangedSubviews: [title, detail])
        texts.axis = .vertical
        texts.spacing = 2
        let head = UIStackView(arrangedSubviews: [icon, texts])
        head.axis = .horizontal
        head.alignment = .firstBaseline
        head.spacing = 10
        let buttons = UIStackView(arrangedSubviews: [primary, secondary, UIView()])
        buttons.axis = .horizontal
        buttons.spacing = 8
        let stack = UIStackView(arrangedSubviews: [head, buttons])
        stack.axis = .vertical
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        glass.contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            glass.leadingAnchor.constraint(equalTo: leadingAnchor),
            glass.trailingAnchor.constraint(equalTo: trailingAnchor),
            glass.topAnchor.constraint(equalTo: topAnchor),
            glass.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: glass.contentView.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: glass.contentView.trailingAnchor, constant: -16),
            stack.topAnchor.constraint(equalTo: glass.contentView.topAnchor, constant: 13),
            stack.bottomAnchor.constraint(equalTo: glass.contentView.bottomAnchor, constant: -13),
        ])
        accessibilityIdentifier = "limit-banner"
    }

    required init?(coder: NSCoder) { fatalError() }

    /// Title and detail for a state (pure, for tests).
    static func text(_ s: State, now: Date) -> (title: String, detail: String) {
        let window = s.window.map { "the \($0) limit" } ?? "a usage limit"
        guard let reset = s.resetAtMs, Double(reset) / 1000 > now.timeIntervalSince1970 else {
            return ("Limited", "Stopped on \(window). It can continue once the window resets.")
        }
        let at = UsageFormat.moment(ms: reset, now: now)
        let left = UsageFormat.countdown(toMs: reset, now: now) ?? "now"
        switch s.recovery {
        case .resumeAtReset: return ("Resumes at \(at)", "Stopped on \(window). Continues the same session in \(left).")
        case .snoozed: return ("Snoozed until \(at)", "Stopped on \(window). Anything queued goes out in \(left).")
        case .ask: return ("Limited · resumes at \(at)", "Stopped on \(window). The window resets in \(left).")
        }
    }

    func update(_ s: State, now: Date = Date()) {
        state = s
        let t = Self.text(s, now: now)
        title.text = t.title
        title.font = Fonts.ui(.sansSemibold, TypeScale.size(15))
        detail.text = t.detail
        detail.font = Fonts.ui(.sans, TypeScale.size(13))
        func set(_ b: UIButton, _ label: String, _ action: LimitAction) {
            var c = b.configuration
            c?.attributedTitle = AttributedString(label, attributes: AttributeContainer([.font: Fonts.ui(.sansSemibold, TypeScale.size(13.5))]))
            b.configuration = c
            // Same identifier: replaces the previous choice's action.
            b.addAction(UIAction(identifier: UIAction.Identifier("limit-choice")) { [weak self] _ in self?.onAction?(action) }, for: .touchUpInside)
            b.isHidden = false
        }
        switch s.recovery {
        case .ask:
            set(primary, "Resume at reset", .resume)
            set(secondary, "Snooze", .snooze)
        case .resumeAtReset, .snoozed:
            set(primary, "Cancel", .ask)
            secondary.isHidden = true
        }
        accessibilityLabel = t.title + ". " + t.detail
    }
}
