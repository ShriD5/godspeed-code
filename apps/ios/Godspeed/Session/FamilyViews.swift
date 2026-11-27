import UIKit

// The session page's multi-session chrome (docs/godspeed/MOBILE-MULTI-SESSION.md):
// the strip of sub-sessions under the navigation bar, the panel that answers
// what waits on you, the sub-session cards painted in the transcript, and the
// "All N" sheet.

// MARK: - Strip

/// Horizontally scrolling glass cards under the navigation bar: the parent
/// (or the breadcrumb up), then every sub-session, waiting-on-you first. The
/// selected card is the page on screen, so the strip doubles as the switch
/// between parent and children.
final class FamilyStripView: UIView {
    var onSelect: ((String) -> Void)?
    var onShowAll: (() -> Void)?

    private let scroller = UIScrollView()
    private var chips: [String: FamilyChipView] = [:]
    private var order: [String] = []
    private var selectedId: String?
    private var revealedSelection = false

    static var height: CGFloat { (58 * TypeScale.factor).rounded() }

    override init(frame: CGRect) {
        super.init(frame: frame)
        scroller.showsHorizontalScrollIndicator = false
        scroller.alwaysBounceHorizontal = true
        scroller.clipsToBounds = false
        scroller.contentInsetAdjustmentBehavior = .never
        scroller.scrollsToTop = false
        addSubview(scroller)
        accessibilityIdentifier = "family-strip"
        shouldGroupAccessibilityChildren = true
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ family: SessionFamily, now: Date = Date()) {
        let entries = family.chips
        var next: [String: FamilyChipView] = [:]
        for chip in entries {
            let view = chips[chip.id] ?? makeChip()
            view.configure(chip, top: family.up == nil, now: now)
            next[chip.id] = view
        }
        for (id, view) in chips where next[id] == nil { view.removeFromSuperview() }
        chips = next
        order = entries.map(\.id)
        selectedId = entries.first { chip in
            switch chip {
            case let .root(_, selected), let .member(_, selected): selected
            default: false
            }
        }?.id
        setNeedsLayout()
    }

    private func makeChip() -> FamilyChipView {
        let v = FamilyChipView()
        v.addAction(UIAction { [weak self, weak v] _ in
            guard let self, let chip = v?.chip else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            switch chip {
            case let .up(m), let .root(m, _), let .member(m, _): self.onSelect?(m.id)
            case .all: self.onShowAll?()
            }
        }, for: .touchUpInside)
        scroller.addSubview(v)
        return v
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        scroller.frame = bounds
        let k = TypeScale.factor
        let inset: CGFloat = 12
        let gap: CGFloat = 8
        var x = inset
        for id in order {
            guard let view = chips[id] else { continue }
            let w = view.preferredWidth(k)
            view.frame = CGRect(x: x, y: 0, width: w, height: bounds.height)
            x += w + gap
        }
        scroller.contentSize = CGSize(width: x - gap + inset, height: bounds.height)
        // Opened on a sub-session far down the list: bring it into view once.
        if !revealedSelection, bounds.width > 0, let id = selectedId, let view = chips[id] {
            revealedSelection = true
            let target = view.frame.insetBy(dx: -inset, dy: 0)
            if target.maxX > bounds.width { scroller.scrollRectToVisible(target, animated: false) }
        }
    }
}

/// One card in the strip.
final class FamilyChipView: UIControl {
    private(set) var chip: SessionFamily.Chip?
    private let glass = Glass.surface(interactive: true, radius: 18)
    private let lead = UIImageView()
    private let glyph = StatusGlyph()
    private let title = FadingLabel()
    private let sub = FadingLabel()
    private let badge = PaddedLabel()
    private let ring = UIView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        glass.isUserInteractionEnabled = false
        addSubview(glass)
        ring.isUserInteractionEnabled = false
        ring.layer.cornerRadius = 18
        ring.layer.cornerCurve = .continuous
        ring.layer.borderWidth = 1.5
        addSubview(ring)
        lead.contentMode = .center
        lead.tintColor = Palette.secondary
        title.textColor = Palette.text
        sub.textColor = Palette.secondary
        badge.textAlignment = .center
        badge.textColor = .white
        badge.backgroundColor = StatusTone.input.withAlphaComponent(1)
        badge.layer.cornerCurve = .continuous
        badge.clipsToBounds = true
        badge.insets = UIEdgeInsets(top: 1, left: 6, bottom: 1, right: 6)
        for v in [lead, glyph, title, sub, badge] as [UIView] { glass.contentView.addSubview(v) }
        isAccessibilityElement = true
        accessibilityTraits = .button
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: FamilyChipView, _) in view.restyle(force: true) }
    }

    required init?(coder: NSCoder) { fatalError() }

    override var isHighlighted: Bool {
        didSet { UIView.animate(withDuration: 0.15) { self.transform = self.isHighlighted ? CGAffineTransform(scaleX: 0.97, y: 0.97) : .identity } }
    }

    func preferredWidth(_ k: CGFloat) -> CGFloat {
        switch chip {
        case .all?: (84 * k).rounded()
        // Narrow enough that the next card peeks in: the strip scrolls.
        default: (158 * k).rounded()
        }
    }

    /// `top`: the family's root has no parent of its own (it reads "Main").
    func configure(_ chip: SessionFamily.Chip, top: Bool, now: Date) {
        self.chip = chip
        let k = TypeScale.factor
        title.font = Fonts.ui(.sansSemibold, TypeScale.size(14))
        sub.font = Fonts.ui(.sans, TypeScale.size(12))
        badge.font = Fonts.ui(.sansSemibold, TypeScale.size(11))
        badge.layer.cornerRadius = (9 * k).rounded()
        let symbol = UIImage.SymbolConfiguration(pointSize: 12 * k, weight: .semibold)
        var waiting = 0
        switch chip {
        case let .up(m):
            lead.image = UIImage(systemName: "chevron.left", withConfiguration: symbol)
            lead.isHidden = false
            glyph.isHidden = true
            title.text = m.title
            sub.text = "Parent · \(m.state.word)"
            accessibilityLabel = "Back to parent, \(m.title), \(m.state.word)"
            accessibilityIdentifier = "family-up"
            isSelected = false
        case let .root(m, selected):
            lead.image = UIImage(systemName: "chevron.left", withConfiguration: symbol)
            lead.isHidden = selected
            glyph.isHidden = !selected
            glyph.kind = m.state.glyph
            let role = top ? "Main" : "Parent"
            title.text = selected ? (top ? "Main" : m.title) : m.title
            sub.text = selected ? (top ? m.title : "This session · \(m.state.word)") : "\(role) · \(m.state.word)"
            accessibilityLabel = selected ? "\(role) session, \(m.title), current" : "Back to \(role.lowercased()) session, \(m.title), \(m.state.word)"
            accessibilityIdentifier = "family-root"
            isSelected = selected
        case let .member(m, selected):
            lead.isHidden = true
            glyph.isHidden = false
            glyph.kind = m.state.glyph
            title.text = m.title
            var line = m.activity
            if let host = m.host { line = "\(host) · \(line)" }
            sub.text = line
            waiting = m.waiting
            var parts = [m.title, m.statusLine(now: now), m.activity]
            if let host = m.host { parts.append("on \(host)") }
            if m.nested > 0 { parts.append("\(m.nested) below") }
            if selected { parts.append("current") }
            accessibilityLabel = parts.joined(separator: ", ")
            accessibilityIdentifier = "family-\(m.id)"
            isSelected = selected
        case let .all(n):
            lead.image = UIImage(systemName: "square.grid.2x2", withConfiguration: symbol)
            lead.isHidden = false
            glyph.isHidden = true
            title.text = "All"
            sub.text = "\(n) sessions"
            accessibilityLabel = "All \(n) sub-sessions"
            accessibilityIdentifier = "family-all"
            isSelected = false
        }
        badge.isHidden = waiting == 0
        badge.text = waiting > 9 ? "9+" : "\(waiting)"
        if waiting > 0 { accessibilityValue = waiting == 1 ? "1 waiting on you" : "\(waiting) waiting on you" } else { accessibilityValue = nil }
        accessibilityTraits = isSelected ? [.button, .selected] : .button
        restyle()
        setNeedsLayout()
    }

    private var styledSelected: Bool?

    private func restyle(force: Bool = false) {
        ring.layer.borderColor = isSelected ? Palette.accent.withAlphaComponent(0.7).cgColor : UIColor.clear.cgColor
        // Re-tinting re-materializes the glass: only when the selection moves.
        guard force || styledSelected != isSelected else { return }
        styledSelected = isSelected
        glass.effect = Glass.effect(interactive: true, tint: isSelected ? Palette.accentSoft : nil)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        glass.frame = bounds
        ring.frame = bounds
        let b = bounds
        let k = TypeScale.factor
        let pad: CGFloat = 12
        let iconSide = (12 * k).rounded()
        let titleH = ceil(title.font.lineHeight)
        let subH = ceil(sub.font.lineHeight)
        let top = ((b.height - titleH - subH - 2) / 2).rounded()
        var x = pad
        if !lead.isHidden {
            lead.frame = CGRect(x: x - 2, y: top, width: iconSide + 4, height: titleH)
            x += iconSide + 6
        } else if !glyph.isHidden {
            glyph.frame = CGRect(x: x, y: top + (titleH - 12) / 2, width: 12, height: 12)
            x += 12 + 7
        }
        var right = b.width - pad
        if !badge.isHidden {
            let s = badge.intrinsicContentSize
            let w = max(s.width, (18 * k).rounded())
            let h = (18 * k).rounded()
            badge.frame = CGRect(x: right - w, y: top + (titleH - h) / 2, width: w, height: h)
            right = badge.frame.minX - 6
        }
        title.frame = CGRect(x: x, y: top, width: max(0, right - x), height: titleH)
        sub.frame = CGRect(x: pad, y: top + titleH + 2, width: b.width - pad * 2, height: subH)
    }
}

// MARK: - What waits on you

/// Above the composer: the oldest approval or question in this session or
/// below it, answered in one tap (the same call as the inbox's swipe).
final class FamilyAskPanel: UIView {
    var onRespond: ((AttentionItemView, Bool) -> Void)?
    var onOpen: ((AttentionItemView) -> Void)?

    private let card = Glass.surface(radius: 24)
    private let header = UILabel()
    private let counter = UIButton(type: .system)
    private let titleLabel = UILabel()
    private let plate = UIView()
    private let edge = UIView()
    private let command = UILabel()
    private let decline = UIButton(type: .system)
    private let approve = UIButton(type: .system)
    private let open = UIButton(type: .system)
    private var items: [AttentionItemView] = []
    private var page = 0
    private var currentId = ""
    private var names: [String: String] = [:]

    override init(frame: CGRect) {
        super.init(frame: frame)
        card.translatesAutoresizingMaskIntoConstraints = false
        addSubview(card)
        header.textColor = Palette.secondary
        header.numberOfLines = 1
        titleLabel.textColor = Palette.text
        titleLabel.numberOfLines = 2
        command.numberOfLines = 3
        command.textColor = Palette.color(.codeText)
        command.lineBreakMode = .byTruncatingTail
        plate.backgroundColor = Palette.codeBackground
        plate.layer.cornerRadius = 10
        plate.layer.cornerCurve = .continuous
        plate.layer.borderWidth = 1
        plate.clipsToBounds = true
        for v in [edge, command] { v.translatesAutoresizingMaskIntoConstraints = false; plate.addSubview(v) }
        NSLayoutConstraint.activate([
            edge.leadingAnchor.constraint(equalTo: plate.leadingAnchor),
            edge.topAnchor.constraint(equalTo: plate.topAnchor),
            edge.bottomAnchor.constraint(equalTo: plate.bottomAnchor),
            edge.widthAnchor.constraint(equalToConstant: 3),
            command.leadingAnchor.constraint(equalTo: plate.leadingAnchor, constant: 12),
            command.trailingAnchor.constraint(equalTo: plate.trailingAnchor, constant: -10),
            command.topAnchor.constraint(equalTo: plate.topAnchor, constant: 8),
            command.bottomAnchor.constraint(equalTo: plate.bottomAnchor, constant: -8),
        ])
        var counterConfig = UIButton.Configuration.plain()
        counterConfig.baseForegroundColor = Palette.secondary
        counterConfig.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 0)
        counter.configuration = counterConfig
        counter.addAction(UIAction { [weak self] _ in self?.advance() }, for: .touchUpInside)
        counter.accessibilityIdentifier = "family-ask-next"

        func button(_ b: UIButton, _ title: String, prominent: Bool, tint: UIColor) {
            var c = prominent ? UIButton.Configuration.prominentGlass() : UIButton.Configuration.glass()
            c.title = title
            c.cornerStyle = .capsule
            c.baseForegroundColor = prominent ? .white : tint
            if prominent { c.baseBackgroundColor = tint }
            c.contentInsets = NSDirectionalEdgeInsets(top: 10, leading: 16, bottom: 10, trailing: 16)
            b.configuration = c
        }
        button(decline, "Decline", prominent: false, tint: Palette.danger)
        button(approve, "Approve", prominent: true, tint: Palette.success)
        button(open, "Open", prominent: false, tint: Palette.text)
        decline.accessibilityIdentifier = "family-ask-decline"
        approve.accessibilityIdentifier = "family-ask-approve"
        open.accessibilityIdentifier = "family-ask-open"
        decline.addAction(UIAction { [weak self] _ in self?.respond(false) }, for: .touchUpInside)
        approve.addAction(UIAction { [weak self] _ in self?.respond(true) }, for: .touchUpInside)
        open.addAction(UIAction { [weak self] _ in
            guard let self, let item = self.item else { return }
            self.onOpen?(item)
        }, for: .touchUpInside)

        let top = UIStackView(arrangedSubviews: [header, counter])
        top.axis = .horizontal
        top.alignment = .center
        top.spacing = 6
        header.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        counter.setContentHuggingPriority(.required, for: .horizontal)
        buttons.addArrangedSubview(open)
        buttons.addArrangedSubview(spacer)
        buttons.addArrangedSubview(decline)
        buttons.addArrangedSubview(approve)
        buttons.spacing = 8
        header.numberOfLines = 2
        layoutButtons()
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self]) { (view: FamilyAskPanel, _) in view.layoutButtons() }
        let stack = UIStackView(arrangedSubviews: [top, titleLabel, plate, buttons])
        stack.axis = .vertical
        stack.spacing = 8
        stack.setCustomSpacing(12, after: plate)
        stack.translatesAutoresizingMaskIntoConstraints = false
        card.contentView.addSubview(stack)
        NSLayoutConstraint.activate([
            card.topAnchor.constraint(equalTo: topAnchor),
            card.bottomAnchor.constraint(equalTo: bottomAnchor),
            card.leadingAnchor.constraint(equalTo: leadingAnchor),
            card.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: card.contentView.topAnchor, constant: 14),
            stack.bottomAnchor.constraint(equalTo: card.contentView.bottomAnchor, constant: -12),
            stack.leadingAnchor.constraint(equalTo: card.contentView.leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(equalTo: card.contentView.trailingAnchor, constant: -12),
        ])
        accessibilityIdentifier = "family-ask"
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: FamilyAskPanel, _) in
            view.plate.layer.borderColor = Palette.codeBorder.cgColor
        }
        plate.layer.borderColor = Palette.codeBorder.cgColor
    }

    required init?(coder: NSCoder) { fatalError() }

    private let buttons = UIStackView()
    private let spacer = UIView()

    /// Side by side; stacked full width at accessibility sizes so "Approve"
    /// never breaks mid-word.
    private func layoutButtons() {
        let stacked = traitCollection.preferredContentSizeCategory.isAccessibilityCategory
        buttons.axis = stacked ? .vertical : .horizontal
        buttons.alignment = stacked ? .fill : .center
        spacer.isHidden = stacked
        if stacked {
            buttons.removeArrangedSubview(approve)
            buttons.insertArrangedSubview(approve, at: 0)
        } else {
            buttons.removeArrangedSubview(approve)
            buttons.addArrangedSubview(approve)
        }
    }

    private var item: AttentionItemView? { items.indices.contains(page) ? items[page] : nil }

    func setGlassVisible(_ visible: Bool, animated: Bool) {
        Glass.setVisible(card, visible, animated: animated)
    }

    /// `names`: session titles by chat id, for "<sub-session> asks".
    func configure(_ waiting: [AttentionItemView], currentId: String, names: [String: String]) {
        let keep = item.map(InboxModel.key)
        guard waiting != items || currentId != self.currentId || names != self.names else { return }
        items = waiting
        self.currentId = currentId
        self.names = names
        page = keep.flatMap { k in waiting.firstIndex { InboxModel.key($0) == k } } ?? 0
        render()
    }

    private func advance() {
        guard items.count > 1 else { return }
        page = (page + 1) % items.count
        UISelectionFeedbackGenerator().selectionChanged()
        render()
    }

    private func respond(_ yes: Bool) {
        guard let item else { return }
        yes ? UINotificationFeedbackGenerator().notificationOccurred(.success) : UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        onRespond?(item, yes)
    }

    private func render() {
        guard let item else { return }
        header.font = Fonts.ui(.sansMedium, TypeScale.size(12.5))
        titleLabel.font = Fonts.ui(.sansSemibold, TypeScale.size(16))
        command.font = Fonts.ui(.mono, TypeScale.size(12.5))
        let who: String? = item.chatId == currentId ? nil : (item.chatId.flatMap { names[$0] } ?? item.title)
        let what = item.kind == .approval ? "needs approval" : "asks"
        header.text = who.map { "\($0) \(what)" } ?? (item.kind == .approval ? "Needs your approval" : "Waiting on your answer")
        var counterConfig = counter.configuration
        counterConfig?.attributedTitle = items.count > 1
            ? AttributedString("\(page + 1) of \(items.count)  ›", attributes: AttributeContainer([.font: Fonts.ui(.sansMedium, TypeScale.size(12.5))]))
            : nil
        counter.configuration = counterConfig
        counter.isHidden = items.count < 2
        counter.accessibilityLabel = "Next, \(page + 1) of \(items.count)"
        titleLabel.text = item.approval?.title ?? (item.summary.isEmpty ? item.title : item.summary)
        if let detail = InboxModel.detail(item) {
            plate.isHidden = false
            command.text = detail
            edge.backgroundColor = InboxModel.riskColor(item.risk)
        } else {
            plate.isHidden = true
        }
        // A question (or a host that can't take answers from the phone) opens the chat.
        approve.isHidden = !item.canApprove
        decline.isHidden = !item.canDecline
        var a = approve.configuration
        a?.title = item.approval?.approveLabel ?? "Approve"
        a?.baseBackgroundColor = InboxModel.riskColor(item.risk) == Palette.danger ? Palette.danger : Palette.success
        approve.configuration = a
        var d = decline.configuration
        d?.title = item.approval?.declineLabel ?? "Decline"
        decline.configuration = d
        open.isHidden = item.chatId == nil || item.chatId == currentId
        if item.chatId != currentId, item.chatId != nil, !item.canApprove {
            open.isHidden = false
        }
        accessibilityLabel = InboxModel.accessibilityLabel(item)
    }
}

// MARK: - Cards in the transcript

/// A sub-session's card, painted natively in the rect the transcript layout
/// reserves for it (`WidgetKind.childCard`). Collapsed: status, title, time,
/// one line of activity. Expanded: host, model, run time, the last message
/// and Open. Tapping the card opens the sub-session.
final class ChildCardView: UIControl {
    var onOpen: ((String) -> Void)?
    var onToggle: ((String) -> Void)?

    private(set) var member: FamilyMember?
    private(set) var expanded = false
    private let plate = UIView()
    private let glyph = StatusGlyph()
    private let title = FadingLabel()
    private let status = UILabel()
    private let activity = FadingLabel()
    private let details = FadingLabel()
    private let body = UILabel()
    private let chevron = UIButton(type: .system)
    private let openButton = UIButton(type: .system)
    private let badge = PaddedLabel()

    /// Lines the layout reserves (rows of `CHILD_LINE` points at text scale):
    /// collapsed, title + activity; expanded, title + details + up to
    /// `bodyLines` of the last message + the Open row.
    static let collapsedLines: UInt32 = 2
    static let bodyLines = 4
    /// Must match `geom::CHILD_PAD` / `CHILD_LINE` in crates/mobile.
    static let pad: CGFloat = 12
    static let line: CGFloat = 20

    override init(frame: CGRect) {
        super.init(frame: frame)
        plate.isUserInteractionEnabled = false
        plate.layer.cornerRadius = 16
        plate.layer.cornerCurve = .continuous
        plate.layer.borderWidth = 1
        addSubview(plate)
        title.textColor = Palette.text
        status.textAlignment = .right
        activity.textColor = Palette.secondary
        details.textColor = Palette.tertiary
        body.textColor = Palette.text
        body.numberOfLines = Self.bodyLines
        body.lineBreakMode = .byTruncatingTail
        badge.textColor = .white
        badge.backgroundColor = StatusTone.input.withAlphaComponent(1)
        badge.clipsToBounds = true
        badge.textAlignment = .center
        badge.insets = UIEdgeInsets(top: 1, left: 6, bottom: 1, right: 6)
        var c = UIButton.Configuration.plain()
        c.baseForegroundColor = Palette.tertiary
        c.contentInsets = .zero
        chevron.configuration = c
        chevron.accessibilityIdentifier = "child-card-toggle"
        chevron.addAction(UIAction { [weak self] _ in
            guard let self, let id = self.member?.id else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            self.onToggle?(id)
        }, for: .touchUpInside)
        var o = UIButton.Configuration.glass()
        o.cornerStyle = .capsule
        o.baseForegroundColor = Palette.text
        o.image = UIImage(systemName: "arrow.up.right")
        o.imagePlacement = .trailing
        o.imagePadding = 4
        o.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 12, bottom: 4, trailing: 12)
        openButton.configuration = o
        openButton.accessibilityIdentifier = "child-card-open"
        openButton.addAction(UIAction { [weak self] _ in
            guard let self, let id = self.member?.id else { return }
            self.onOpen?(id)
        }, for: .touchUpInside)
        glyph.isUserInteractionEnabled = false
        for v in [glyph, title, status, badge, activity, details, body, chevron, openButton] as [UIView] { addSubview(v) }
        addAction(UIAction { [weak self] _ in
            guard let self, let id = self.member?.id else { return }
            self.onOpen?(id)
        }, for: .touchUpInside)
        isAccessibilityElement = true
        accessibilityTraits = .button
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: ChildCardView, _) in view.restyle() }
        restyle()
    }

    required init?(coder: NSCoder) { fatalError() }

    override var isHighlighted: Bool {
        didSet { plate.backgroundColor = isHighlighted ? Palette.controlFill : Palette.userBubble }
    }

    private func restyle() {
        plate.backgroundColor = isHighlighted ? Palette.controlFill : Palette.userBubble
        plate.layer.borderColor = (member?.state == .needsYou ? StatusTone.input : Palette.codeBorder).cgColor
    }

    /// The transcript's text scale (rows lay out at body-size ratio).
    private var scale: CGFloat { Self.scale }
    static var scale: CGFloat { UIFontMetrics(forTextStyle: .body).scaledValue(for: 17) / 17 }

    /// How many lines the card needs at a column `width`.
    static func lines(_ m: FamilyMember, expanded: Bool, width: CGFloat) -> UInt32 {
        guard expanded else { return collapsedLines }
        let s = scale
        let font = Fonts.ui(.sans, 13.5 * s)
        let textWidth = max(40, width - (14 * s + 12 + 9 * s) - 12 * s)
        let text = (m.preview ?? m.activity) as NSString
        let height = text.boundingRect(with: CGSize(width: textWidth, height: .greatestFiniteMagnitude), options: [.usesLineFragmentOrigin], attributes: [.font: font], context: nil).height
        let body = min(bodyLines, max(1, Int(ceil(height / max(1, font.lineHeight)))))
        return UInt32(3 + body)
    }

    func configure(_ m: FamilyMember, expanded: Bool, now: Date = Date()) {
        member = m
        self.expanded = expanded
        let s = scale
        title.font = Fonts.ui(.sansSemibold, 15 * s)
        status.font = Fonts.ui(.sansMedium, 12.5 * s)
        activity.font = Fonts.ui(.sans, 13.5 * s)
        details.font = Fonts.ui(.sans, 12.5 * s)
        body.font = Fonts.ui(.sans, 13.5 * s)
        badge.font = Fonts.ui(.sansSemibold, 11 * s)
        badge.layer.cornerRadius = 9 * s
        glyph.kind = m.state.glyph
        title.text = m.title
        status.text = m.statusLine(now: now)
        status.textColor = m.state == .idle ? StatusTone.time : m.state.tint.withAlphaComponent(1)
        activity.text = m.activity
        details.text = m.details(now: now)
        body.text = m.preview ?? m.activity
        badge.isHidden = m.waiting == 0
        badge.text = m.waiting > 9 ? "9+" : "\(m.waiting)"
        var c = chevron.configuration
        c?.image = UIImage(systemName: expanded ? "chevron.up" : "chevron.down", withConfiguration: UIImage.SymbolConfiguration(pointSize: 11 * s, weight: .semibold))
        chevron.configuration = c
        chevron.accessibilityLabel = expanded ? "Show less" : "Show more"
        var o = openButton.configuration
        o?.attributedTitle = AttributedString("Open", attributes: AttributeContainer([.font: Fonts.ui(.sansSemibold, 13 * s)]))
        o?.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(pointSize: 11 * s, weight: .semibold)
        openButton.configuration = o
        for v in [details, body, openButton] as [UIView] { v.isHidden = !expanded }
        activity.isHidden = expanded
        accessibilityLabel = ["Sub-session", m.title, m.statusLine(now: now), m.activity, expanded ? m.details(now: now) : nil].compactMap { $0 }.joined(separator: ", ")
        accessibilityHint = "Opens the sub-session"
        accessibilityIdentifier = "child-card-\(m.id)"
        accessibilityCustomActions = [UIAccessibilityCustomAction(name: expanded ? "Show less" : "Show more") { [weak self] _ in
            guard let id = self?.member?.id else { return false }
            self?.onToggle?(id)
            return true
        }]
        restyle()
        setNeedsLayout()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let b = bounds
        plate.frame = b
        let s = scale
        let pad = Self.pad * s
        let line = Self.line * s
        let x0 = 14 * s
        let right = b.width - 12 * s
        // Row 1: status, title, time, chevron.
        var y = pad
        glyph.frame = CGRect(x: x0, y: y + (line - 12) / 2, width: 12, height: 12)
        let chevronSide = 28 * s
        chevron.frame = CGRect(x: right - chevronSide + 6 * s, y: y + (line - chevronSide) / 2, width: chevronSide, height: chevronSide)
        let sw = min(ceil(status.sizeThatFits(CGSize(width: b.width, height: line)).width), b.width * 0.42)
        status.frame = CGRect(x: chevron.frame.minX - sw - 2 * s, y: y, width: sw, height: line)
        var titleRight = status.frame.minX - 8 * s
        if !badge.isHidden {
            let bs = badge.intrinsicContentSize
            let bw = max(bs.width, 18 * s)
            let bh = 18 * s
            badge.frame = CGRect(x: titleRight - bw, y: y + (line - bh) / 2, width: bw, height: bh)
            titleRight = badge.frame.minX - 6 * s
        }
        let tx = glyph.frame.maxX + 9 * s
        title.frame = CGRect(x: tx, y: y, width: max(0, titleRight - tx), height: line)
        y += line
        if !expanded {
            activity.frame = CGRect(x: tx, y: y, width: max(0, right - tx), height: line)
            return
        }
        details.frame = CGRect(x: tx, y: y, width: max(0, right - tx), height: line)
        y += line
        // Whatever the layout reserved past title, details and the Open row.
        let shown = max(1, Int(((b.height - pad * 2) / line).rounded()) - 3)
        body.numberOfLines = shown
        body.frame = CGRect(x: tx, y: y, width: max(0, right - tx), height: line * CGFloat(shown))
        y += line * CGFloat(shown)
        let ow = openButton.intrinsicContentSize.width
        openButton.frame = CGRect(x: tx - 12 * s, y: y - line * 0.1, width: ow, height: line * 1.3)
    }
}

// MARK: - "All N" sheet

/// Every sub-session in one column, grouped by what it needs, big rows.
final class FamilySheetViewController: UITableViewController {
    var onSelect: ((String) -> Void)?
    private var sections: [(FamilyMember.State, [FamilyMember])] = []
    private let rootTitle: String

    init(family: SessionFamily) {
        rootTitle = family.root.title
        super.init(style: .insetGrouped)
        title = "\(family.members.count) sub-sessions"
        apply(family)
    }

    required init?(coder: NSCoder) { fatalError() }

    func apply(_ family: SessionFamily) {
        let groups = Dictionary(grouping: family.members, by: \.state)
        sections = groups.keys.sorted().map { ($0, groups[$0] ?? []) }
        if isViewLoaded { tableView.reloadData() }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        navigationItem.prompt = rootTitle
        tableView.backgroundColor = Palette.background
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "m")
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .close, primaryAction: UIAction { [weak self] _ in self?.dismiss(animated: true) })
        tableView.accessibilityIdentifier = "family-sheet"
    }

    override func numberOfSections(in tableView: UITableView) -> Int { sections.count }

    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { sections[section].1.count }

    override func tableView(_ tableView: UITableView, titleForHeaderInSection section: Int) -> String? {
        let (state, list) = sections[section]
        return "\(state.word) · \(list.count)"
    }

    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "m", for: indexPath)
        let m = sections[indexPath.section].1[indexPath.row]
        var c = UIListContentConfiguration.subtitleCell()
        c.text = m.title
        c.textProperties.font = Fonts.ui(.sansSemibold, TypeScale.size(16))
        c.textProperties.color = Palette.text
        var second = m.statusLine(now: Date())
        if let host = m.host { second += " · \(host)" }
        second += "\n" + m.activity
        c.secondaryText = second
        c.secondaryTextProperties.font = Fonts.ui(.sans, TypeScale.size(13.5))
        c.secondaryTextProperties.color = Palette.secondary
        c.secondaryTextProperties.numberOfLines = 3
        c.image = UIImage(systemName: Self.symbol(m.state))
        c.imageProperties.tintColor = m.state.tint.withAlphaComponent(1)
        c.textToSecondaryTextVerticalPadding = 4
        c.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 12, leading: 16, bottom: 12, trailing: 16)
        cell.contentConfiguration = c
        cell.accessoryType = .disclosureIndicator
        cell.backgroundColor = Palette.elevated
        cell.accessibilityIdentifier = "family-sheet-\(m.id)"
        return cell
    }

    override func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
        let id = sections[indexPath.section].1[indexPath.row].id
        dismiss(animated: true) { [onSelect] in onSelect?(id) }
    }

    static func symbol(_ state: FamilyMember.State) -> String {
        switch state {
        case .needsYou: "exclamationmark.bubble.fill"
        case .working: "circle.dotted"
        case .failed: "xmark.octagon.fill"
        case .done: "checkmark.circle.fill"
        case .idle: "circle"
        }
    }
}
